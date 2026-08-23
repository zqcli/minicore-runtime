use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::value::BoundedText;

use super::types::ToolValueError;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ToolInputRequest {
    pub prompt: BoundedText,
    pub choices: Vec<BoundedText>,
    pub answer_kind: ToolInputAnswerKind,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolInputRequestWire {
    prompt: BoundedText,
    choices: Vec<BoundedText>,
    answer_kind: ToolInputAnswerKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputAnswerKind {
    Text,
    SingleChoice,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ToolInputAnswer {
    Text(BoundedText),
    Choice { index: usize },
}

#[derive(Serialize)]
struct CanonicalTextResult<'a> {
    answer: &'a str,
}

#[derive(Serialize)]
struct CanonicalChoiceResult<'a> {
    choice_index: usize,
    choice: &'a str,
}

impl<'de> Deserialize<'de> for ToolInputRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = ToolInputRequestWire::deserialize(deserializer)?;
        Self::new(value.prompt.as_str(), value.choices, value.answer_kind)
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for ToolInputAnswer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| D::Error::custom("tool input answer must be an object"))?;
        if object.len() != 2 {
            return Err(D::Error::custom("tool input answer has unknown fields"));
        }
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| D::Error::custom("tool input answer kind must be a string"))?;
        let data = object
            .get("data")
            .ok_or_else(|| D::Error::custom("tool input answer data is missing"))?;
        match kind {
            "text" => {
                let text = data
                    .as_str()
                    .ok_or_else(|| D::Error::custom("tool input text answer must be a string"))?;
                validate_answer_text(text).map_err(D::Error::custom)?;
                Ok(Self::Text(
                    BoundedText::new_with_max_bytes(text, 8_192).map_err(D::Error::custom)?,
                ))
            }
            "choice" => {
                let choice = data.as_object().ok_or_else(|| {
                    D::Error::custom("tool input choice answer must be an object")
                })?;
                if choice.len() != 1 || !choice.contains_key("index") {
                    return Err(D::Error::custom("tool input choice has unknown fields"));
                }
                let index = choice.get("index").and_then(Value::as_u64).ok_or_else(|| {
                    D::Error::custom("tool input choice index must be an integer")
                })?;
                let index = usize::try_from(index)
                    .map_err(|_| D::Error::custom("tool input choice index is too large"))?;
                Ok(Self::Choice { index })
            }
            _ => Err(D::Error::custom("unknown tool input answer kind")),
        }
    }
}

impl ToolInputRequest {
    pub fn new(
        prompt: impl AsRef<str>,
        choices: Vec<BoundedText>,
        answer_kind: ToolInputAnswerKind,
    ) -> Result<Self, ToolValueError> {
        let prompt = BoundedText::new_with_max_bytes(prompt.as_ref(), 8_192)
            .map_err(|_| ToolValueError::InvalidText)?;
        if !valid_input_text(prompt.as_str(), 8_192, false)
            || choices.len() > 32
            || choices
                .iter()
                .any(|choice| !valid_input_text(choice.as_str(), 1_024, false))
            || (matches!(answer_kind, ToolInputAnswerKind::SingleChoice) && choices.is_empty())
        {
            return Err(ToolValueError::InvalidText);
        }
        Ok(Self {
            prompt,
            choices,
            answer_kind,
        })
    }

    pub const fn prompt(&self) -> &BoundedText {
        &self.prompt
    }

    pub fn choices(&self) -> &[BoundedText] {
        &self.choices
    }

    pub const fn answer_kind(&self) -> ToolInputAnswerKind {
        self.answer_kind
    }
}

impl ToolInputAnswer {
    pub fn validate(&self, request: &ToolInputRequest) -> Result<(), ToolValueError> {
        match (self, request.answer_kind) {
            (Self::Text(value), ToolInputAnswerKind::Text)
                if validate_answer_text(value.as_str()).is_ok() =>
            {
                Ok(())
            }
            (Self::Choice { index }, ToolInputAnswerKind::SingleChoice)
                if *index < request.choices.len() =>
            {
                Ok(())
            }
            _ => Err(ToolValueError::InvalidAnswer),
        }
    }

    pub(crate) fn encode_result(
        &self,
        request: &ToolInputRequest,
    ) -> Result<String, ToolValueError> {
        self.validate(request)?;
        let result = match self {
            Self::Text(answer) => serde_json::to_string(&CanonicalTextResult {
                answer: answer.as_str(),
            }),
            Self::Choice { index } => {
                let choice = request
                    .choices()
                    .get(*index)
                    .ok_or(ToolValueError::InvalidAnswer)?;
                serde_json::to_string(&CanonicalChoiceResult {
                    choice_index: *index,
                    choice: choice.as_str(),
                })
            }
        };
        result.map_err(|_| ToolValueError::InvalidAnswer)
    }
}

fn valid_input_text(value: &str, maximum: usize, allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty())
        && value.len() <= maximum
        && value.chars().all(|character| !character.is_control())
}

fn validate_answer_text(value: &str) -> Result<(), ToolValueError> {
    if valid_input_text(value, 8_192, false) {
        Ok(())
    } else {
        Err(ToolValueError::InvalidAnswer)
    }
}

#[cfg(test)]
#[test]
fn canonical_result_struct_uses_stable_json_escaping() {
    let encoded = serde_json::to_string(&CanonicalTextResult {
        answer: "quote\" slash\\ newline\n tab\t",
    })
    .unwrap();
    assert_eq!(encoded, r#"{"answer":"quote\" slash\\ newline\n tab\t"}"#);
}
