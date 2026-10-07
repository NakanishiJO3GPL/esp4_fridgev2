//! 中央領域の平均輝度(APL)が安定したとき、静止画JPEG化要求を生成する。
//! 判定は取得タスクで行い、通知にはDMAバッファのポインタを含めない。

#[derive(Clone, Copy, Debug)]
pub struct AplTriggerConfig {
    /// APL差分の絶対値がこの値以下なら安定と判定する。
    pub settle_velocity_abs_max: f32,
    /// 発火に必要な連続安定差分の数。0でも最初の安定差分が必要。
    pub settle_frames_required: usize,
    /// 発火後、このAPL差分絶対値以上になるまで再アームしない。
    pub wait_to_detect_velocity_abs_min: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TriggerState {
    Detecting,
    ForceOffNextFrame,
    HoldOffUntilRearm,
}

/// JPEG化要求のメタデータ。画像そのものやバッファの所有権は含まない。
/// 発火したフレームをJPEG化する場合、Frameが解放される前に別途保存が必要。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JpegTriggerNotification {
    pub frame_sequence: u32,
    pub center_apl: f32,
    pub velocity: f32,
}

/// APLのフレーム間差分を監視するワンショット判定器。
/// 初回は履歴の保存だけを行う。発火翌フレームと再アームフレームは必ずOFF。
#[derive(Debug)]
pub struct AplTrigger {
    cfg: AplTriggerConfig,
    prev_apl: Option<f32>,
    settle_count: usize,
    state: TriggerState,
    last_velocity: Option<f32>,
}

impl AplTrigger {
    pub fn new(cfg: AplTriggerConfig) -> Self {
        Self {
            cfg,
            prev_apl: None,
            settle_count: 0,
            state: TriggerState::Detecting,
            last_velocity: None,
        }
    }

    /// 3領域の中央(index 1)だけを判定し、発火時にJPEG化要求を返す。
    /// 各取得フレームで1回呼ぶ。計算失敗時はresetして履歴を破棄する。
    pub fn update_center(
        &mut self,
        frame_sequence: u32,
        means: &[f32; 3],
    ) -> Option<JpegTriggerNotification> {
        self.update(means[1]).then(|| JpegTriggerNotification {
            frame_sequence,
            center_apl: means[1],
            velocity: self.last_velocity(),
        })
    }

    /// 発火フレームだけtrueを返す。非有限値は履歴を初期化し発火しない。
    pub fn update(&mut self, apl: f32) -> bool {
        if !apl.is_finite() {
            self.reset();
            return false;
        }
        let prev = self.prev_apl.replace(apl);
        let Some(prev) = prev else {
            return false;
        };
        let velocity = apl - prev;
        self.last_velocity = Some(velocity);
        let velocity_abs = velocity.abs();

        match self.state {
            TriggerState::Detecting => {
                if !(velocity_abs <= self.cfg.settle_velocity_abs_max) {
                    self.settle_count = 0;
                    return false;
                }
                self.settle_count = self.settle_count.saturating_add(1);
                if self.settle_count >= self.cfg.settle_frames_required {
                    self.state = TriggerState::ForceOffNextFrame;
                    self.settle_count = 0;
                    true
                } else {
                    false
                }
            }
            TriggerState::ForceOffNextFrame | TriggerState::HoldOffUntilRearm => {
                self.state = if velocity_abs >= self.cfg.wait_to_detect_velocity_abs_min {
                    TriggerState::Detecting
                } else {
                    TriggerState::HoldOffUntilRearm
                };
                false
            }
        }
    }

    pub fn last_velocity(&self) -> f32 {
        self.last_velocity.unwrap_or(0.0)
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            TriggerState::Detecting => "検出中",
            TriggerState::ForceOffNextFrame => "強制オフ",
            TriggerState::HoldOffUntilRearm => "待ち",
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new(self.cfg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> AplTriggerConfig {
        AplTriggerConfig {
            settle_velocity_abs_max: 0.125,
            settle_frames_required: 2,
            wait_to_detect_velocity_abs_min: 0.5,
        }
    }

    #[test]
    fn center_region_generates_notification_after_consecutive_stable_differences() {
        let mut trigger = AplTrigger::new(cfg());
        assert_eq!(trigger.update_center(1, &[0.0, 10.0, 255.0]), None);
        assert_eq!(trigger.update_center(2, &[255.0, 10.125, 0.0]), None);
        assert_eq!(
            trigger.update_center(3, &[0.0, 10.25, 255.0]),
            Some(JpegTriggerNotification {
                frame_sequence: 3,
                center_apl: 10.25,
                velocity: 0.125,
            })
        );
    }

    #[test]
    fn holds_off_until_rearm_and_rearm_frame_does_not_fire() {
        let mut trigger = AplTrigger::new(cfg());
        for (apl, fire, state) in [
            (10.0, false, "検出中"),
            (10.0, false, "検出中"),
            (10.0, true, "強制オフ"),
            (10.0, false, "待ち"),
            (10.25, false, "待ち"),
            (10.75, false, "検出中"),
            (10.75, false, "検出中"),
            (10.75, true, "強制オフ"),
        ] {
            assert_eq!(trigger.update(apl), fire);
            assert_eq!(trigger.state_name(), state);
        }
    }

    #[test]
    fn next_frame_can_rearm_but_never_fire() {
        let mut config = cfg();
        config.settle_velocity_abs_max = 0.5;
        config.settle_frames_required = 1;
        for direction in [-1.0, 1.0] {
            let mut trigger = AplTrigger::new(config);
            assert!(!trigger.update(10.0));
            assert!(trigger.update(10.0));
            assert!(!trigger.update(10.0 + direction * 0.5));
            assert_eq!(trigger.state_name(), "検出中");
            assert!(trigger.update(10.0 + direction));
        }
    }

    #[test]
    fn unstable_difference_breaks_consecutive_count() {
        let mut trigger = AplTrigger::new(cfg());
        for (apl, fire) in [
            (10.0, false),
            (10.125, false),
            (11.0, false),
            (11.0, false),
            (11.0, true),
        ] {
            assert_eq!(trigger.update(apl), fire);
        }
    }

    #[test]
    fn non_finite_input_and_reset_clear_history() {
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut trigger = AplTrigger::new(cfg());
            assert!(!trigger.update(10.0));
            assert!(!trigger.update(10.0));
            assert!(!trigger.update(invalid));
            assert_eq!(trigger.last_velocity(), 0.0);
            assert!(!trigger.update(20.0));
            assert!(!trigger.update(20.0));
            assert!(trigger.update(20.0));
            trigger.reset();
            assert_eq!(trigger.state_name(), "検出中");
            assert_eq!(trigger.last_velocity(), 0.0);
            assert!(!trigger.update(20.0));
        }
    }

    #[test]
    fn zero_required_count_still_needs_a_stable_difference() {
        let mut config = cfg();
        config.settle_frames_required = 0;
        let mut trigger = AplTrigger::new(config);
        assert!(!trigger.update(10.0));
        assert!(!trigger.update(11.0));
        assert!(trigger.update(11.125));
    }

    #[test]
    fn monitors_velocity_and_inclusive_thresholds_in_both_directions() {
        for direction in [-1.0, 1.0] {
            let mut trigger = AplTrigger::new(cfg());
            assert!(!trigger.update(10.0));
            assert!(!trigger.update(10.0 + direction * 0.125));
            assert_eq!(trigger.last_velocity(), direction * 0.125);
            assert!(trigger.update(10.0 + direction * 0.25));
            assert!(!trigger.update(10.0 + direction * 0.75));
            assert_eq!(trigger.last_velocity(), direction * 0.5);
            assert_eq!(trigger.state_name(), "検出中");
        }
    }

    #[test]
    fn settle_counter_saturates() {
        let mut config = cfg();
        config.settle_frames_required = usize::MAX;
        let mut trigger = AplTrigger::new(config);
        assert!(!trigger.update(10.0));
        trigger.settle_count = usize::MAX;
        assert!(trigger.update(10.0));
        assert_eq!(trigger.settle_count, 0);
    }

    #[test]
    fn full_notification_queue_does_not_block() {
        use std::sync::mpsc::{sync_channel, TrySendError};
        let (sender, receiver) = sync_channel(1);
        let notification = JpegTriggerNotification {
            frame_sequence: 1,
            center_apl: 10.0,
            velocity: 0.0,
        };
        sender.try_send(notification).unwrap();
        assert_eq!(
            sender.try_send(notification),
            Err(TrySendError::Full(notification))
        );
        assert_eq!(receiver.recv().unwrap(), notification);
    }
}
