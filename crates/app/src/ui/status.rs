use data_relay_core::config::{self, Config};
use tokio::sync::oneshot;

/// A successful probe belongs to the exact input that produced it. Changing
/// the input also drops an outstanding reply, so late results cannot be shown
/// for a different destination.
#[derive(Default)]
pub(super) struct ConnectionProbe {
    base: Option<String>,
    receiver: Option<oneshot::Receiver<Result<(), String>>>,
    pub result: Option<Result<(), String>>,
}

impl ConnectionProbe {
    pub fn invalidate_changed_base(&mut self, current: &str) {
        if self.base.as_deref().is_some_and(|base| base != current) {
            *self = Self::default();
        }
    }

    pub fn begin(&mut self, base: String) -> oneshot::Sender<Result<(), String>> {
        let (sender, receiver) = oneshot::channel();
        self.base = Some(base);
        self.receiver = Some(receiver);
        self.result = None;
        sender
    }

    pub fn pending(&self) -> bool {
        self.receiver.is_some()
    }

    pub fn receive(&mut self, current: &str) {
        self.invalidate_changed_base(current);
        let Some(receiver) = self.receiver.as_mut() else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => result,
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => {
                Err("연결 테스트 응답이 중단되었습니다.".into())
            }
        };
        self.receiver = None;
        self.result = Some(result);
    }
}

pub(super) fn connection_settings_pending(draft: &Config, applied: &Config) -> bool {
    (draft.equip != applied.equip || draft.server != applied.server)
        && draft.equip_id().is_ok()
        && config::valid_unc(&draft.server.unc_base)
}

pub(super) fn is_unapplied_connection_error(error: &str, draft: &Config, applied: &Config) -> bool {
    connection_settings_pending(draft, applied)
        && (error.starts_with("equip.")
            || error.starts_with("equip_id:")
            || error.starts_with("server.unc_base:"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_destination_discards_success_and_late_reply() {
        let mut probe = ConnectionProbe::default();
        let reply = probe.begin("first".into());
        reply.send(Ok(())).unwrap();
        probe.receive("second");
        assert!(probe.result.is_none());
        assert!(!probe.pending());
        let reply = probe.begin("second".into());
        probe.invalidate_changed_base("third");
        assert!(reply.send(Ok(())).is_err());
        let reply = probe.begin("third".into());
        reply.send(Ok(())).unwrap();
        probe.receive("third");
        assert!(matches!(probe.result, Some(Ok(()))));
        probe.invalidate_changed_base("fourth");
        assert!(probe.result.is_none());
    }

    #[test]
    fn corrected_unsaved_connection_is_pending_without_masking_other_errors() {
        let applied = Config::default();
        let mut draft = applied.clone();
        draft.equip.num = "A".into();
        draft.equip.name = "press".into();
        draft.server.unc_base = r"\\server\share".into();
        assert!(connection_settings_pending(&draft, &applied));
        assert!(is_unapplied_connection_error(
            "equip.num: required",
            &draft,
            &applied
        ));
        assert!(!is_unapplied_connection_error(
            "watch[0].pattern: invalid",
            &draft,
            &applied
        ));
        assert!(!connection_settings_pending(&draft, &draft));
        draft.server.unc_base.clear();
        assert!(!connection_settings_pending(&draft, &applied));
    }
}
