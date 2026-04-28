use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const PIPE_NAME: &str = "super-tabs:update";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdatePayload {
    pub version: u8,
    pub pane_id: u32,
    pub updates: BTreeMap<String, String>,
}

impl UpdatePayload {
    pub fn parse(payload: &str) -> Result<Self, String> {
        let parsed: Self = serde_json::from_str(payload)
            .map_err(|error| format!("invalid update payload: {error}"))?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn to_json(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(self).map_err(|error| format!("failed to encode payload: {error}"))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("unsupported payload version {}", self.version));
        }
        if self.updates.is_empty() {
            return Err("payload must include at least one column update".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_round_trip_validates() {
        let payload = UpdatePayload {
            version: 1,
            pane_id: 12,
            updates: BTreeMap::from([("branch".to_string(), "main".to_string())]),
        };

        let json = payload.to_json().unwrap();
        assert_eq!(UpdatePayload::parse(&json).unwrap(), payload);
    }
}
