"""증분 CSV(.csv.gz) 파일에서 컬럼명과 값을 읽는 최소 예제.

사용법:
    python examples/read_incremental_csv.py data/increment-001.csv.gz

모든 값은 CSV에서 읽은 문자열 그대로 반환합니다. 숫자나 시간으로
자동 변환하지 않으므로, 적재할 때 필요한 타입 변환은 호출하는 쪽에서
명시적으로 처리하면 됩니다.
"""

from __future__ import annotations

import argparse
import csv
import gzip
from collections.abc import Iterator
from pathlib import Path


def read_rows(path: Path, *, encoding: str = "utf-8-sig") -> Iterator[dict[str, str]]:
    """압축된 증분 CSV를 한 행씩 읽어 컬럼명-값 매핑을 반환한다."""
    with gzip.open(path, mode="rt", encoding=encoding, newline="") as stream:
        reader = csv.DictReader(stream)

        # 첫 행이 헤더이므로, 헤더 이름을 먼저 확인한다.
        fieldnames = reader.fieldnames
        if not fieldnames:
            raise ValueError("CSV 헤더가 없거나 비어 있습니다.")
        if any(name is None or not name.strip() for name in fieldnames):
            raise ValueError(f"비어 있는 컬럼명이 있습니다: {fieldnames!r}")
        if len(set(fieldnames)) != len(fieldnames):
            raise ValueError(f"중복된 컬럼명이 있습니다: {fieldnames!r}")

        for line_number, row in enumerate(reader, start=2):
            # 헤더보다 값이 적으면 DictReader가 누락 값을 None으로 채운다.
            missing = [name for name, value in row.items() if value is None]
            # 헤더보다 값이 많으면 남는 값이 None 키 아래에 모인다.
            extra = row.get(None)
            if missing or extra is not None:
                details = []
                if missing:
                    details.append(f"누락 컬럼={missing!r}")
                if extra is not None:
                    details.append(f"추가 값={extra!r}")
                raise ValueError(f"{line_number}행의 컬럼 수가 맞지 않습니다: {', '.join(details)}")

            # row.items()에서 column_name은 컬럼명, value는 그 컬럼의 값이다.
            # 값은 문자열이며, 이 지점에서 QuestDB용 변환/매핑 로직을 붙이면 된다.
            yield {column_name: value for column_name, value in row.items()}


def read_rows_list(path: Path, *, encoding: str = "utf-8-sig") -> list[dict[str, str]]:
    """증분 CSV의 모든 행을 딕셔너리 리스트로 읽어 반환합니다."""
    return list(read_rows(path, encoding=encoding))


def main() -> None:
    parser = argparse.ArgumentParser(description="증분 CSV(.csv.gz)의 컬럼명과 값을 출력합니다.")
    parser.add_argument("file", type=Path, help="읽을 .csv.gz 파일 경로")
    args = parser.parse_args()

    rows = read_rows_list(args.file)
    for row_number, row in enumerate(rows, start=1):
        print(f"row {row_number}:")
        for column_name, value in row.items():
            print(f"  {column_name} = {value!r}")


if __name__ == "__main__":
    main()
