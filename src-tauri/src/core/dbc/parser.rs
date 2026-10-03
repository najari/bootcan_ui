//! DBC loader using the same pinned candb-engine as canlog (codex_test).
use super::models::*;
use candb_engine::{analysis, codec::CompiledMessage, model, parser::Document};
use std::{collections::HashMap, io::Read, path::Path, sync::Arc};

pub struct DbcParser;
const MAX_DBC_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> DbcDatabase {
        DbcParser::parse(include_str!("../../../../tests/fixtures/integration.dbc")).unwrap()
    }
    #[test]
    fn engine_distinguishes_small_extended_ids_and_scopes_value_tables() {
        let db = fixture();
        assert!(db.engine.is_some());
        assert_eq!(db.messages.len(), 7);
        let std = db.decode_message(256, &[2, 254]);
        assert_eq!(std[0].physical_value, -9.0);
        assert_eq!(std[0].value_name.as_deref(), Some("StandardOnly"));
        assert_eq!(std[1].physical_value, -2.0);
        let ext = db.decode_message(0x80000100, &[0, 2]);
        assert_eq!(ext[0].physical_value, 2.0);
        assert_eq!(ext[0].value_name.as_deref(), Some("ExtendedOnly"));
        assert!(db.decode_message(0x80000200, &[1, 2, 3]).is_empty());
    }
    #[test]
    fn engine_respects_multiplex_pages_and_extended_ranges() {
        let db = fixture();
        let names: Vec<_> = db
            .decode_message(512, &[2, 42, 5])
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(names, ["Selector", "PageTwo", "Always"]);
        assert!(db.decode_signal(512, "PageOne", &[2, 42, 5]).is_none());
        assert_eq!(
            db.decode_signal(513, "Ranged", &[3, 77])
                .unwrap()
                .physical_value,
            77.0
        );
        assert!(db.decode_signal(513, "Ranged", &[4, 77]).is_none());
    }
    #[test]
    fn engine_encodes_motorola_signed_and_fractional_float() {
        let db = fixture();
        assert_eq!(
            db.encode_signals(
                0x80000100,
                &HashMap::from([("Shared".into(), 0x1234 as f64)]),
                None
            )
            .unwrap(),
            [0x12, 0x34]
        );
        assert_eq!(
            db.encode_signals(
                256,
                &HashMap::from([("Signed".into(), -2.0)]),
                Some(vec![9, 0])
            )
            .unwrap(),
            [9, 254]
        );
        let bytes = db
            .encode_signals(514, &HashMap::from([("Value".into(), 1.25)]), None)
            .unwrap();
        assert_eq!(bytes, 1.25f32.to_le_bytes());
        assert_eq!(db.decode_message(514, &bytes)[0].physical_value, 1.25);
        assert!(db
            .encode_signals(256, &HashMap::from([("Signed".into(), 256.0)]), None)
            .is_err());
    }
    #[test]
    fn engine_preserves_u64_raw_text_and_fd_bits_above_255() {
        let db = fixture();
        assert_eq!(
            db.decode_message(515, &[255; 8])[0].raw_text.as_deref(),
            Some("18446744073709551615")
        );
        let mut bytes = vec![0; 64];
        bytes[63] = 127;
        assert_eq!(db.decode_message(516, &bytes)[0].raw_value, 127);
        assert_eq!(db.messages[&516].signals[0].start_bit, 504);
        assert!(db.decode_message(516, &bytes[..8]).is_empty());
    }
    #[test]
    fn rejects_malformed_empty_duplicate_and_invalid_can_definitions() {
        for content in [
            "invalid garbage",
            "VERSION \"empty\"",
            "BU_: ECU\nBO_ 2048 Invalid: 1 ECU\n",
            "BU_: ECU\nBO_ 256 A: 1 ECU\nBO_ 256 B: 1 ECU\n",
            "BU_: ECU\nBO_ 256 A: 1 ECU\n SG_ TooWide : 0|16@1+ (1,0) [0|65535] \"\" ECU\n",
        ] {
            assert!(DbcParser::parse(content).is_err(), "accepted {content}");
        }
    }
}

impl DbcParser {
    pub fn parse_file<P: AsRef<Path>>(path: P) -> Result<DbcDatabase, String> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|e| e.to_string())?
            .take(MAX_DBC_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Self::parse_bytes(&bytes)
    }

    pub fn parse(content: &str) -> Result<DbcDatabase, String> {
        Self::parse_bytes(content.as_bytes())
    }

    pub fn parse_bytes(bytes: &[u8]) -> Result<DbcDatabase, String> {
        if bytes.len() as u64 > MAX_DBC_BYTES {
            return Err("DBC exceeds 32 MiB limit".into());
        }
        let document = Document::from_bytes(bytes, None)?;
        if !document.db.is_can_bus() {
            return Err("DBC describes a non-CAN bus".into());
        }
        let findings = analysis::check(&document);
        let errors: Vec<_> = document
            .warnings
            .iter()
            .chain(findings.iter())
            .filter(|f| f.severity == "error")
            .collect();
        if !errors.is_empty() {
            return Err(format!(
                "Invalid DBC: {}",
                serde_json::to_string(&errors).map_err(|e| e.to_string())?
            ));
        }
        if !document.warnings.is_empty() {
            return Err(format!(
                "DBC parse findings: {}",
                serde_json::to_string(&document.warnings).map_err(|e| e.to_string())?
            ));
        }
        let mut db = DbcDatabase::new();
        db.version = Some(document.db.version.clone());
        db.nodes = document.db.nodes.iter().map(|n| n.name.clone()).collect();
        let mut compiled = HashMap::new();
        for message in &document.db.messages {
            if message.raw_id == 0xc000_0000 {
                continue;
            }
            if message.dlc > 64
                || message.raw_id & 0x6000_0000 != 0
                || (!message.extended() && message.raw_id > 0x7ff)
            {
                return Err(format!(
                    "Invalid single-frame CAN definition: {}",
                    message.name
                ));
            }
            if compiled.contains_key(&message.raw_id) {
                return Err(format!("Duplicate DBC ID: {}", message.raw_id));
            }
            let mut signals = Vec::new();
            for signal in &message.signals {
                let table_name = format!("{}:{}", message.raw_id, signal.name);
                if !signal.values.is_empty() {
                    db.value_tables.insert(
                        table_name.clone(),
                        ValueTable {
                            name: table_name.clone(),
                            values: signal.values.iter().cloned().collect(),
                        },
                    );
                }
                signals.push(Signal {
                    name: signal.name.clone(),
                    start_bit: signal.start,
                    length: signal.length,
                    byte_order: if signal.byte_order.is_intel() {
                        ByteOrder::LittleEndian
                    } else {
                        ByteOrder::BigEndian
                    },
                    value_type: match signal.value_type {
                        model::ValueType::Float => ValueType::Float,
                        model::ValueType::Double => ValueType::Double,
                        _ if signal.signed => ValueType::Signed,
                        _ => ValueType::Unsigned,
                    },
                    factor: signal.factor,
                    offset: signal.offset,
                    minimum: Some(signal.minimum),
                    maximum: Some(signal.maximum),
                    unit: signal.unit.clone(),
                    receivers: signal.receivers.clone(),
                    comment: (!signal.comment.is_empty()).then(|| signal.comment.clone()),
                    value_table: (!signal.values.is_empty()).then_some(table_name),
                    is_multiplexer: signal.mux.acts_as_switch(),
                    multiplex: signal.mux.value(),
                });
            }
            db.messages.insert(
                message.raw_id,
                Message {
                    id: message.raw_id,
                    name: message.name.clone(),
                    dlc: message.dlc as u8,
                    sender: Some(message.transmitter.clone()),
                    signals,
                    comment: (!message.comment.is_empty()).then(|| message.comment.clone()),
                },
            );
            compiled.insert(message.raw_id, CompiledMessage::compile(message));
        }
        if compiled.is_empty() {
            return Err("DBC has no CAN message definitions".into());
        }
        db.engine = Some(Arc::new(compiled));
        Ok(db)
    }
}
