use crate::core::message::CanFrame;
use serde::{Deserialize, Serialize};

/// Filter rule for CAN messages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilterRule {
    /// Filter by ID range (inclusive)
    IdRange { min: u32, max: u32 },
    /// Filter by exact ID
    IdExact(u32),
    /// Filter by data pattern (byte positions and values)
    DataPattern { pattern: Vec<DataByteMatch> },
    /// Filter by DLC
    DlcRange { min: u8, max: u8 },
    /// Filter by direction
    Direction { rx: bool, tx: bool },
    /// Filter by extended ID flag
    ExtendedId(bool),
    /// Filter by remote frame flag
    RemoteFrame(bool),
    /// 1-based source bus number (or the Net's configured bus number).
    Channel(u16),
    /// Seconds from the file/connection origin, [start, end).
    TimeRange {
        start: Option<f64>,
        end: Option<f64>,
    },
}

/// Data byte match specification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataByteMatch {
    pub position: u8,
    pub value: u8,
    pub mask: u8, // Bit mask (0xFF = exact match, 0x00 = don't care)
}

/// Filter set with logical operators
#[derive(Clone, Serialize, Deserialize)]
pub struct FilterSet {
    pub rules: Vec<FilterRule>,
    pub logic: FilterLogic,
    #[serde(skip)]
    prepared: Option<std::sync::Arc<Vec<PreparedRule>>>,
    #[serde(skip)]
    buses: std::collections::HashMap<String, u16>,
}

enum PreparedRule {
    Engine(canlog::app::Filter),
    Native(FilterRule),
}

fn input_args() -> canlog::app::InputArgs {
    canlog::app::InputArgs {
        input: String::new(),
        input_format: None,
        id_map: None,
        preserve_records: false,
        unsupported: canlog::app::Unsupported::Skip,
        recover: false,
        channel: vec![],
        id: vec![],
        id_kind: None,
        direction: None,
        start: None,
        end: None,
        limit: None,
        report: None,
        max_line_bytes: 65536,
        max_object_bytes: 1048576,
        max_container_bytes: 8388608,
    }
}

/// Logical operator for combining filter rules
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilterLogic {
    And, // All rules must match
    Or,  // Any rule must match
}

impl FilterRule {
    /// Check if a frame matches this filter rule
    pub fn matches(&self, frame: &CanFrame) -> bool {
        match self {
            FilterRule::IdRange { min, max } => frame.id >= *min && frame.id <= *max,
            FilterRule::IdExact(id) => frame.id == *id,
            FilterRule::DataPattern { pattern } => pattern.iter().all(|match_spec| {
                if (match_spec.position as usize) < frame.data.len() {
                    let byte = frame.data[match_spec.position as usize];
                    (byte & match_spec.mask) == (match_spec.value & match_spec.mask)
                } else {
                    false
                }
            }),
            FilterRule::DlcRange { min, max } => frame.dlc >= *min && frame.dlc <= *max,
            FilterRule::Direction { rx, tx } => {
                (frame.direction == "rx" && *rx) || (frame.direction == "tx" && *tx)
            }
            FilterRule::ExtendedId(extended) => frame.is_extended == *extended,
            FilterRule::RemoteFrame(remote) => frame.is_remote == *remote,
            FilterRule::Channel(bus) => frame.bus == Some(*bus),
            FilterRule::TimeRange { start, end } => {
                frame.timestamp >= start.unwrap_or(0.0)
                    && frame.timestamp < end.unwrap_or(f64::INFINITY)
            }
        }
    }
}

impl FilterSet {
    /// Create a new filter set
    pub fn new(rules: Vec<FilterRule>, logic: FilterLogic) -> Self {
        Self {
            rules,
            logic,
            prepared: None,
            buses: Default::default(),
        }
    }

    /// Compile once when rules change; ID/channel/direction/time semantics are canlog's.
    pub fn prepare(
        mut self,
        buses: std::collections::HashMap<String, u16>,
    ) -> Result<Self, String> {
        let mut prepared = Vec::new();
        for rule in &self.rules {
            let mut args = input_args();
            match rule {
                FilterRule::IdRange { min, max } => args.id.push(format!("0x{min:X}-0x{max:X}")),
                FilterRule::IdExact(id) => args.id.push(format!("0x{id:X}")),
                FilterRule::ExtendedId(ext) => {
                    args.id_kind = Some(if *ext {
                        canlog::app::IdKind::Extended
                    } else {
                        canlog::app::IdKind::Standard
                    })
                }
                FilterRule::Direction { rx, tx } if rx != tx => {
                    args.direction = Some(if *rx {
                        canlog::core::Direction::Rx
                    } else {
                        canlog::core::Direction::Tx
                    })
                }
                FilterRule::Channel(bus) => args.channel.push(*bus),
                FilterRule::TimeRange { start, end } => {
                    let to_ns = |v: f64| -> Result<i64, String> {
                        if !v.is_finite() || v < 0.0 || v >= i64::MAX as f64 / 1e9 {
                            return Err("Invalid filter time".into());
                        }
                        Ok((v * 1e9).round() as i64)
                    };
                    args.start = start.map(to_ns).transpose()?;
                    args.end = end.map(to_ns).transpose()?;
                }
                FilterRule::DlcRange { min, max } if min > max || *max > 64 => {
                    return Err("Invalid DLC range".into())
                }
                _ => {
                    prepared.push(PreparedRule::Native(rule.clone()));
                    continue;
                }
            }
            prepared.push(PreparedRule::Engine(
                canlog::app::Filter::new(args).map_err(|e| format!("{e:#}"))?,
            ));
        }
        self.prepared = Some(std::sync::Arc::new(prepared));
        self.buses = buses;
        Ok(self)
    }

    /// Check if a frame matches the filter set
    pub fn matches(&self, frame: &CanFrame) -> bool {
        if self.rules.is_empty() {
            return true; // No filters = match all
        }

        let fallback;
        let prepared = if let Some(prepared) = &self.prepared {
            prepared
        } else {
            fallback = match self.clone().prepare(Default::default()) {
                Ok(f) => f,
                Err(_) => return false,
            };
            fallback.prepared.as_ref().unwrap()
        };
        let bus = self.buses.get(&frame.channel).copied().unwrap_or(1);
        let converted = super::log_adapter::as_log_frame(frame, bus).ok();
        let matches = |rule: &PreparedRule| match rule {
            PreparedRule::Engine(filter) => converted.as_ref().is_some_and(|f| filter.matches(f)),
            PreparedRule::Native(rule) => rule.matches(frame),
        };
        match self.logic {
            FilterLogic::And => prepared.iter().all(matches),
            FilterLogic::Or => prepared.iter().any(matches),
        }
    }

    /// Check if filter set is empty (no filtering)
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Default for FilterSet {
    fn default() -> Self {
        Self {
            rules: vec![],
            logic: FilterLogic::And,
            prepared: None,
            buses: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_filter_combines_engine_and_byte_rules_at_time_boundaries() {
        let mut frame = CanFrame {
            id: 0x123,
            dlc: 1,
            data: vec![0xab],
            channel: "net-a".into(),
            timestamp: 0.2,
            timestamp_ns: Some(200_000_000),
            ..Default::default()
        };
        let filter = FilterSet::new(
            vec![
                FilterRule::Channel(16),
                FilterRule::TimeRange {
                    start: Some(0.2),
                    end: Some(0.3),
                },
                FilterRule::DataPattern {
                    pattern: vec![DataByteMatch {
                        position: 0,
                        value: 0xa0,
                        mask: 0xf0,
                    }],
                },
            ],
            FilterLogic::And,
        )
        .prepare(std::collections::HashMap::from([("net-a".into(), 16)]))
        .unwrap();
        assert!(filter.matches(&frame));
        frame.timestamp_ns = Some(300_000_000);
        assert!(!filter.matches(&frame));
        frame.timestamp_ns = Some(199_999_999);
        assert!(!filter.matches(&frame));
        frame.timestamp_ns = Some(250_000_000);
        frame.data[0] = 0xb0;
        assert!(!filter.matches(&frame));
        let either = FilterSet::new(
            vec![FilterRule::IdExact(1), FilterRule::Channel(2)],
            FilterLogic::Or,
        )
        .prepare(Default::default())
        .unwrap();
        frame.bus = Some(2);
        assert!(either.matches(&frame));
        frame.bus = Some(3);
        assert!(!either.matches(&frame));
    }

    #[test]
    fn invalid_rules_are_rejected_before_replacing_active_filter() {
        for rule in [
            FilterRule::IdRange { min: 5, max: 1 },
            FilterRule::IdExact(0x20000000),
            FilterRule::Channel(0),
            FilterRule::TimeRange {
                start: Some(2.0),
                end: Some(1.0),
            },
            FilterRule::TimeRange {
                start: Some(f64::NAN),
                end: None,
            },
            FilterRule::DlcRange { min: 0, max: 65 },
        ] {
            assert!(FilterSet::new(vec![rule], FilterLogic::And)
                .prepare(Default::default())
                .is_err());
        }
    }

    #[test]
    fn test_id_range_filter() {
        let filter = FilterRule::IdRange {
            min: 0x100,
            max: 0x200,
        };
        let frame1 = CanFrame {
            id: 0x150,
            ..Default::default()
        };
        let frame2 = CanFrame {
            id: 0x300,
            ..Default::default()
        };

        assert!(filter.matches(&frame1));
        assert!(!filter.matches(&frame2));
    }

    #[test]
    fn test_data_pattern_filter() {
        let filter = FilterRule::DataPattern {
            pattern: vec![DataByteMatch {
                position: 0,
                value: 0x01,
                mask: 0xFF,
            }],
        };
        let mut frame1 = CanFrame::default();
        frame1.data = vec![0x01, 0x02, 0x03];
        let mut frame2 = CanFrame::default();
        frame2.data = vec![0x02, 0x02, 0x03];

        assert!(filter.matches(&frame1));
        assert!(!filter.matches(&frame2));
    }

    #[test]
    fn test_filter_set_and() {
        let filter_set = FilterSet::new(
            vec![
                FilterRule::IdRange {
                    min: 0x100,
                    max: 0x200,
                },
                FilterRule::Direction {
                    rx: true,
                    tx: false,
                },
            ],
            FilterLogic::And,
        );

        let mut frame1 = CanFrame::default();
        frame1.id = 0x150;
        frame1.direction = "rx".to_string();

        let mut frame2 = CanFrame::default();
        frame2.id = 0x150;
        frame2.direction = "tx".to_string();

        assert!(filter_set.matches(&frame1));
        assert!(!filter_set.matches(&frame2));
    }
}
