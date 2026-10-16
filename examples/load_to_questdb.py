"""UNC 공유 폴더의 증분 CSV 조각(.csv.gz)을 QuestDB에 long 형식으로 적재하는 예제.

CSV 한 행(타임스탬프 + 데이터 컬럼 N개)을 데이터 컬럼마다 한 행씩, N행으로 풀어 넣는다.

    timestamp | EQUIP_NUM | EQUIP_NAME | filename | colname | value_int | value_float | value_str

- EQUIP_NUM, EQUIP_NAME: 에이전트가 올리는 하트비트 `_status/{equip_id}.json`에서 읽는다.
  조각 경로의 equip_id는 구분자 때문에 번호와 이름을 정확히 나눌 수 없어서다.
- filename: 원본 CSV 파일 이름(확장자 포함). 하위 폴더는 넣지 않는다.
- 값 타입: 정수 → value_int, 실수 → value_float, 나머지(빈 값 포함) → value_str.
- 타임스탬프 컬럼: 이름이 timestamp/time/ts/datetime 등인 컬럼, 없으면 첫 컬럼.
  시간대가 없는 값은 --tz-offset(기본 +09:00)으로 해석한다.

사용법:
    pip install "questdb>=2,<4"
    python examples/load_to_questdb.py \\\\server\\share --host 192.168.88.250

같은 조각을 다시 넣어도 테이블의 DEDUP 키가 중복을 흡수한다. 적재가 끝난 조각은
--state 파일에 기록해 다음 실행 때 건너뛴다.
"""

from __future__ import annotations

import argparse
import csv
import gzip
import io
import json
import re
import sys
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path

from questdb.ingress import Sender, TimestampNanos

TABLE_DDL = """CREATE TABLE IF NOT EXISTS {table} (
    timestamp TIMESTAMP,
    EQUIP_NUM SYMBOL,
    EQUIP_NAME SYMBOL,
    filename SYMBOL,
    colname SYMBOL,
    value_int LONG,
    value_float DOUBLE,
    value_str VARCHAR
) TIMESTAMP(timestamp) PARTITION BY DAY WAL
DEDUP UPSERT KEYS(timestamp, EQUIP_NUM, EQUIP_NAME, filename, colname)"""

TIMESTAMP_NAMES = {"timestamp", "time", "ts", "datetime", "date_time", "일시", "시간", "시각"}
INT_RE = re.compile(r"[+-]?\d+")
FLOAT_RE = re.compile(r"[+-]?(\d+\.\d*|\.\d+|\d+)([eE][+-]?\d+)?")
I64_MIN, I64_MAX = -(2**63), 2**63 - 1
# 에이전트가 이름 충돌·파일 재생성 때 붙이는 접미사 (crates/core/src/spool.rs, state.rs).
COLLISION_SUFFIX = re.compile(r"~c[0-9a-f]{16}(-\d+)?$")
GENERATION_SUFFIX = re.compile(r"~g\d+-[0-9a-f]{16}$")


def parse_chunk_name(name: str) -> tuple[str, int, int, str]:
    """`{name_seg}__{start}-{end}__{hdr|syn}.csv.gz` → (name_seg, start, end, kind)."""
    stem = name.removesuffix(".csv.gz")
    name_range, kind = stem.rsplit("__", 1)
    name_seg, offsets = name_range.rsplit("__", 1)
    start, end = (int(x) for x in offsets.split("-", 1))
    if kind not in ("hdr", "syn"):
        raise ValueError(f"알 수 없는 헤더 종류: {kind}")
    return name_seg, start, end, kind


def source_filename(name_seg: str) -> str:
    """name_seg(`LineA~Vib_001.CSV`)에서 원본 파일 이름(`Vib_001.CSV`)을 꺼낸다."""
    seg = COLLISION_SUFFIX.sub("", name_seg)
    seg = GENERATION_SUFFIX.sub("", seg)
    last = seg.rsplit("~", 1)[-1]
    return last.replace("%7e", "~").replace("%25", "%")


def classify(value: str) -> tuple[str, int | float | str]:
    """정수 → int, 실수 → float, 나머지 → str. 앞뒤 공백은 판정에서만 무시한다."""
    text = value.strip()
    if INT_RE.fullmatch(text):
        number = int(text)
        if I64_MIN <= number <= I64_MAX:
            return "value_int", number
        return "value_str", value
    if FLOAT_RE.fullmatch(text):
        return "value_float", float(text)
    return "value_str", value


def parse_timestamp(text: str, tz: timezone) -> datetime:
    value = datetime.fromisoformat(text.strip().replace("/", "-"))
    if value.tzinfo is None:
        value = value.replace(tzinfo=tz)
    return value


def timestamp_nanos(value: datetime) -> TimestampNanos:
    delta = value - datetime(1970, 1, 1, tzinfo=timezone.utc)
    nanos = (delta.days * 86_400 + delta.seconds) * 1_000_000_000 + delta.microseconds * 1_000
    return TimestampNanos(nanos)


def decode(raw: bytes) -> str:
    try:
        return raw.decode("utf-8-sig")
    except UnicodeDecodeError:
        return raw.decode("cp949")


def guess_delimiter(header_line: str) -> str:
    counts = {d: header_line.count(d) for d in (",", "\t", ";", "|")}
    best = max(counts, key=counts.get)
    return best if counts[best] else ","


def load_equipments(share: Path) -> dict[str, tuple[str, str]]:
    """하트비트 파일에서 equip_id → (장비 번호, 장비 이름)."""
    equipments = {}
    for path in (share / "_status").glob("*.json"):
        try:
            status = json.loads(path.read_text(encoding="utf-8"))
            equipments[status["equip_id"]] = (status["equip_num"], status["equip_name"])
        except (OSError, ValueError, KeyError) as error:
            print(f"하트비트를 읽지 못했습니다: {path}: {error}", file=sys.stderr)
    return equipments


def chunk_rows(path: Path, tz: timezone):
    """조각 하나를 (timestamp, colname, 값 컬럼, 값) 단위로 풀어서 내보낸다."""
    text = decode(gzip.decompress(path.read_bytes()))
    first_line = text.split("\n", 1)[0]
    reader = csv.reader(io.StringIO(text, newline=""), delimiter=guess_delimiter(first_line))
    header = next(reader)
    ts_index = next((i for i, name in enumerate(header) if name.strip().lower() in TIMESTAMP_NAMES), 0)
    for line_number, row in enumerate(reader, start=2):
        if not row:
            continue
        if len(row) != len(header):
            raise ValueError(f"{line_number}행 컬럼 수 {len(row)} ≠ 헤더 {len(header)}")
        ts = timestamp_nanos(parse_timestamp(row[ts_index], tz))
        for index, (colname, value) in enumerate(zip(header, row)):
            if index != ts_index:
                column, typed = classify(value)
                yield ts, colname, column, typed


def create_table(host: str, port: int, table: str) -> None:
    query = urllib.parse.urlencode({"query": TABLE_DDL.format(table=table)})
    with urllib.request.urlopen(f"http://{host}:{port}/exec?{query}", timeout=30) as response:
        body = json.load(response)
    if "error" in body:
        raise RuntimeError(f"테이블 생성 실패: {body['error']}")


def main() -> int:
    parser = argparse.ArgumentParser(description="증분 CSV 조각을 QuestDB에 long 형식으로 적재합니다.")
    parser.add_argument("share", type=Path, help="UNC 공유 루트 (raw\\, _status\\ 가 있는 폴더)")
    parser.add_argument("--host", default="192.168.88.250")
    parser.add_argument("--port", type=int, default=9000, help="QuestDB HTTP 포트 (ILP·SQL 공용)")
    parser.add_argument("--table", default="csv_edge_values")
    parser.add_argument("--state", type=Path, default=Path("loaded_chunks.txt"), help="적재 완료 조각 기록 파일")
    parser.add_argument("--tz-offset", default="+09:00", help="시간대 없는 타임스탬프의 UTC 오프셋")
    args = parser.parse_args()

    sign = -1 if args.tz_offset.startswith("-") else 1
    hours, minutes = (int(x) for x in args.tz_offset.lstrip("+-").split(":"))
    tz = timezone(sign * timedelta(hours=hours, minutes=minutes))

    create_table(args.host, args.port, args.table)
    equipments = load_equipments(args.share)
    loaded = set(args.state.read_text(encoding="utf-8").splitlines()) if args.state.exists() else set()

    chunks = sorted(
        (path for path in (args.share / "raw").glob("*/*/*.csv.gz")),
        key=lambda p: (p.parent.parent.name, parse_chunk_name(p.name)[0], parse_chunk_name(p.name)[1]),
    )
    done = failed = rows = 0
    with Sender.from_conf(f"http::addr={args.host}:{args.port};") as sender, args.state.open("a", encoding="utf-8") as state:
        for path in chunks:
            key = path.relative_to(args.share).as_posix()
            if key in loaded:
                continue
            equip_id = path.parent.parent.name
            if equip_id not in equipments:
                print(f"건너뜀(하트비트 없음, 다음 실행 때 다시 시도): {key}", file=sys.stderr)
                continue
            equip_num, equip_name = equipments[equip_id]
            # 조각마다 버퍼를 따로 둬서, 깨진 조각의 일부 행만 들어가는 일이 없게 한다.
            buffer = sender.new_buffer()
            try:
                filename = source_filename(parse_chunk_name(path.name)[0])
                count = 0
                for ts, colname, column, value in chunk_rows(path, tz):
                    buffer.row(
                        args.table,
                        symbols={"EQUIP_NUM": equip_num, "EQUIP_NAME": equip_name, "filename": filename, "colname": colname},
                        columns={column: value},
                        at=ts,
                    )
                    count += 1
                sender.flush(buffer)
            except Exception as error:  # 조각 하나가 깨져도 나머지는 계속 넣는다.
                print(f"실패: {key}: {error}", file=sys.stderr)
                failed += 1
                continue
            state.write(key + "\n")
            state.flush()
            done += 1
            rows += count
    print(f"적재 조각 {done}개, 행 {rows}개, 실패 {failed}개")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
