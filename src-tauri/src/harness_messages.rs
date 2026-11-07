use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessDeliveryRequest {
    pub session_id: u64,
    pub native_session_id: Option<String>,
    pub idempotency_key: String,
    pub host_id: String,
    pub model_id: String,
    pub project: Option<String>,
    pub text: String,
}

impl HarnessDeliveryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.session_id == 0 {
            return Err("session_id must be greater than zero".into());
        }
        if let Some(native_session_id) = self.native_session_id.as_deref() {
            validate_identifier("native_session_id", native_session_id)?;
        }
        validate_identifier("idempotency_key", &self.idempotency_key)?;
        validate_identifier("host_id", &self.host_id)?;
        validate_identifier("model_id", &self.model_id)?;
        if self
            .project
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err("project cannot be empty".into());
        }
        if self.text.trim().is_empty() {
            return Err("message text cannot be empty".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessDeliveryResponse {
    pub reply: String,
    pub native_session_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HarnessSessionArchiveRequest {
    pub native_session_id: String,
    pub archived: bool,
}

impl HarnessSessionArchiveRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_identifier("native_session_id", &self.native_session_id)
    }
}

fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{name} cannot be empty"))
    } else if value.len() > 512 {
        Err(format!("{name} is too long"))
    } else {
        Ok(())
    }
}
