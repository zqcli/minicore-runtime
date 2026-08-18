use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use super::super::{Tool, ToolContext, ToolFuture, ToolSpec};
use super::{failure, success};

const INVALID_ARGUMENTS: &str = "tool arguments are invalid";
const DESCRIPTION: &str = "Ask the user a question and optionally provide a list of choices.";

#[derive(Clone, Copy, Debug, Default)]
pub struct AskUserTool;

impl AskUserTool {
    pub const fn new() -> Self {
        Self
    }

    fn arguments_are_valid(arguments: &AskUserArguments) -> bool {
        if arguments.question.is_empty()
            || arguments.question.len() > 8 * 1024
            || arguments.question.chars().any(char::is_control)
        {
            return false;
        }
        arguments.choices.as_ref().is_none_or(|choices| {
            !choices.is_empty()
                && choices.len() <= 32
                && choices.iter().all(|choice| {
                    !choice.is_empty()
                        && choice.len() <= 1024
                        && !choice.chars().any(char::is_control)
                })
        })
    }
}

impl Tool for AskUserTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "ask_user".parse().expect("builtin name is valid"),
            DESCRIPTION,
            json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": 8192
                    },
                    "choices": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 32,
                        "items": {
                            "type": "string",
                            "minLength": 1,
                                "maxLength": 1024
                        }
                    }
                },

                "required": ["question"],
                "additionalProperties": false
            }),
        )
        .expect("builtin spec is valid")
    }

    fn execute<'a>(&'a self, ctx: ToolContext<'a>, args: Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let arguments = match serde_json::from_value::<AskUserArguments>(args) {
                Ok(arguments) if Self::arguments_are_valid(&arguments) => arguments,
                _ => return failure(INVALID_ARGUMENTS),
            };
            let answer = ctx.ask_user(arguments.question, arguments.choices).await?;
            success(answer.text().to_owned())
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserArguments {
    question: String,
    #[serde(default, deserialize_with = "deserialize_choices")]
    choices: Option<Vec<String>>,
}

fn deserialize_choices<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    if value.is_null() {
        return Err(D::Error::custom("choices must be an array"));
    }
    Vec::<String>::deserialize(value)
        .map(Some)
        .map_err(D::Error::custom)
}
