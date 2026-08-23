use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::de::Error as _;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::ids::{SessionId, SessionInstanceId, ToolCallId, TurnId};
use crate::value::{BoundedText, MAX_TEXT_BYTES};

use super::context::ToolContext;
use super::input::ToolInputRequest;
use super::types::{ToolError, ToolName, ToolValueError, valid_text, validate_json_shape};

#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ToolSpec {
    pub name: ToolName,
    pub description: BoundedText,
    pub input_schema: Value,
}

impl fmt::Debug for ToolSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let schema_bytes = serde_json::to_vec(&self.input_schema).map_or(0, |bytes| bytes.len());
        let schema_object_keys = self
            .input_schema
            .as_object()
            .map_or(0, |object| object.len());
        formatter
            .debug_struct("ToolSpec")
            .field("name", &self.name)
            .field("description_bytes", &self.description.byte_len())
            .field("schema_object_keys", &schema_object_keys)
            .field("schema_bytes", &schema_bytes)
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolSpecWire {
    name: ToolName,
    description: BoundedText,
    input_schema: Value,
}

impl ToolSpec {
    pub fn new(
        name: ToolName,
        description: impl AsRef<str>,
        input_schema: Value,
    ) -> Result<Self, ToolValueError> {
        let description = BoundedText::new_with_max_bytes(description.as_ref(), 4_096)
            .map_err(|_| ToolValueError::InvalidText)?;
        let spec = Self {
            name,
            description,
            input_schema,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), ToolValueError> {
        if !valid_text(self.description.as_str(), 4_096, false) {
            return Err(ToolValueError::InvalidText);
        }
        if !self.input_schema.is_object() || !validate_json_shape(&self.input_schema) {
            return Err(ToolValueError::InvalidSchema);
        }
        Ok(())
    }

    pub const fn name(&self) -> &ToolName {
        &self.name
    }

    pub const fn description(&self) -> &BoundedText {
        &self.description
    }

    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }
}

impl<'de> Deserialize<'de> for ToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolSpecWire::deserialize(deserializer)?;
        Self::new(value.name, value.description, value.input_schema)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ToolOutput {
    pub content: BoundedText,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolOutputWire {
    content: BoundedText,
}

impl ToolOutput {
    pub fn new(content: impl AsRef<str>) -> Result<Self, ToolValueError> {
        let content = content.as_ref();
        if !valid_text(content, MAX_TEXT_BYTES, true) {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self {
            content: BoundedText::new_with_max_bytes(content, MAX_TEXT_BYTES)
                .map_err(|_| ToolValueError::InvalidText)?,
        })
    }

    pub const fn content(&self) -> &BoundedText {
        &self.content
    }
}

impl fmt::Debug for ToolOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolOutput")
            .field("content_bytes", &self.content.byte_len())
            .finish()
    }
}

impl Serialize for ToolOutput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("ToolOutput", 1)?;
        state.serialize_field("content", &self.content)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for ToolOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolOutputWire::deserialize(deserializer)?;
        Self::new(value.content.as_str()).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocation {
    pub session_id: SessionId,
    pub instance_id: SessionInstanceId,
    pub turn_id: TurnId,
    pub tool_call_id: ToolCallId,
    pub tool_name: ToolName,
    pub arguments: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolInvocationWire {
    session_id: SessionId,
    instance_id: SessionInstanceId,
    turn_id: TurnId,
    tool_call_id: ToolCallId,
    tool_name: ToolName,
    arguments: Value,
}

impl ToolInvocation {
    pub fn new(
        session_id: SessionId,
        instance_id: SessionInstanceId,
        turn_id: TurnId,
        tool_call_id: ToolCallId,
        tool_name: ToolName,
        arguments: Value,
    ) -> Result<Self, ToolError> {
        if !arguments.is_object() || !validate_json_shape(&arguments) {
            return Err(ToolError::InvalidInvocation);
        }
        Ok(Self {
            session_id,
            instance_id,
            turn_id,
            tool_call_id,
            tool_name,
            arguments,
        })
    }

    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub const fn instance_id(&self) -> SessionInstanceId {
        self.instance_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn tool_call_id(&self) -> &ToolCallId {
        &self.tool_call_id
    }

    pub const fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }

    pub const fn arguments(&self) -> &Value {
        &self.arguments
    }
}

impl fmt::Debug for ToolInvocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolInvocation")
            .field("session_id", &self.session_id)
            .field("instance_id", &self.instance_id)
            .field("turn_id", &self.turn_id)
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("arguments", &"<redacted>")
            .finish()
    }
}

impl<'de> Deserialize<'de> for ToolInvocation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolInvocationWire::deserialize(deserializer)?;
        Self::new(
            value.session_id,
            value.instance_id,
            value.turn_id,
            value.tool_call_id,
            value.tool_name,
            value.arguments,
        )
        .map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolExecutionOutcome {
    Completed(ToolOutput),
    RequestInput(ToolInputRequest),
}

pub type ToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionOutcome, ToolError>> + Send + 'a>>;

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> &ToolSpec;

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a>;
}

impl<T: Tool + ?Sized> Tool for Arc<T> {
    fn spec(&self) -> &ToolSpec {
        (**self).spec()
    }

    fn execute<'a>(&'a self, invocation: ToolInvocation, context: ToolContext) -> ToolFuture<'a> {
        (**self).execute(invocation, context)
    }
}
