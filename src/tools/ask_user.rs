//! The M14 production `ask_user` builtin: one closed, default-off, Runtime-owned Tool.
//!
//! The builtin is immutable after `open` and travels through the existing residency ToolSet
//! capture path (`MiniCoreRuntime::open` selects exactly one ToolSet — the empty default or
//! the opt-in `ask_user` builtin — and installs it once).  It is a production Tool slice,
//! not an OS-backed Sandbox completion: it truthfully requires zero
//! `ToolCapabilityClass` permissions and uses the available empty sandbox contract.
//!
//! A call plans synchronously to one of exactly two shapes:
//!
//! - `ToolExecutionPlan::UserQuestion` for a valid call: the typed `UserQuestionRequest` is
//!   built through the existing semantic constructors (authoritative for byte/count/index
//!   validation), and the move-only `UserQuestionAnswerBinding` re-validates the typed host
//!   answer through `UserQuestionRequest::validate_answer` before projecting it as one
//!   deterministic compact JSON Text part under `PreExecution + Succeeded`.  The builtin
//!   never creates a `ToolExecutionStart`, an executor future, a cancellation pair, a
//!   start-gate reservation, an approval, or any OS resource.
//! - `ToolExecutionPlan::PreExecution` with the frozen `Failed` text
//!   `tool arguments are invalid` for any parse or semantic failure, with no Interaction.
//!
//! Arguments are parsed from `BoundedJsonObject::canonical_json()` with private strict serde
//! mirrors that reject unknown fields at every layer; omitted/null `title` both map to
//! `None`; question/option indices must be strictly increasing but need not start at zero or
//! be contiguous.  The input schema disclosed to the model is closed
//! (`additionalProperties: false` everywhere) but is guidance only — the semantic
//! constructors are the authority.
//!
//! The answer projection is bounded by the existing `ToolResultContent` contract: the
//! projection's canonical envelope is identical to `user_answer_encoded_len` (same fixed
//! literals, serde escaping never exceeds the canonical escaping count), so a validated
//! answer always renders within the 65,536-byte single-part limit.  Any dynamic
//! invariant/render failure fails closed to `Abandoned { RuntimeFailure }` and can never
//! emit malformed model-visible output.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::wire::BoundedJsonObject;

use super::{
    ToolAbandonReason, ToolDefinition, ToolExecutionMode, ToolExecutionPlan, ToolExecutionRequest,
    ToolExecutionResult, ToolResultContent, ToolResultDisposition, ToolSandboxContract, ToolSet,
    ToolSetInner, ToolSpec, ToolValueError, UserQuestionAnswer, UserQuestionAnswerBinding,
    UserQuestionAnswerValue, UserQuestionChoice, UserQuestionField, UserQuestionInput,
    UserQuestionRequest,
};

/// The exact production builtin ToolName.  `pub(super)` because the composed production
/// ToolSet routes exactly this frozen name.
pub(super) const ASK_USER_NAME: &str = "ask_user";

/// The exact production description disclosed for the builtin.
const ASK_USER_DESCRIPTION: &str = "Ask the user one or more non-secret text or single-choice questions and return the answers. Use only when the task cannot continue without user input. Never request passwords, API keys, tokens, credentials, or other secrets.";

/// The exact frozen PreExecution Failed text for every parse or semantic argument failure.
const INVALID_ARGUMENTS_TEXT: &str = "tool arguments are invalid";

/// The closed input schema disclosed for the builtin.  Structural guidance only: byte/count/
/// index semantics are enforced by the existing owner constructors, never by this schema.
const ASK_USER_SCHEMA: &str = r#"{
  "type": "object",
  "properties": {
    "title": {
      "oneOf": [
        {
          "type": "string",
          "minLength": 1,
          "maxLength": 256
        },
        {
          "type": "null"
        }
      ]
    },
    "questions": {
      "type": "array",
      "minItems": 1,
      "maxItems": 32,
      "items": {
        "type": "object",
        "properties": {
          "questionIndex": {
            "type": "integer",
            "minimum": 0,
            "maximum": 4294967295
          },
          "prompt": {
            "type": "string",
            "minLength": 1,
            "maxLength": 8192
          },
          "required": {
            "type": "boolean"
          },
          "input": {
            "oneOf": [
              {
                "type": "object",
                "properties": {
                  "type": {
                    "type": "string",
                    "const": "text"
                  },
                  "data": {
                    "type": "object",
                    "properties": {
                      "multiline": {
                        "type": "boolean"
                      }
                    },
                    "required": ["multiline"],
                    "additionalProperties": false
                  }
                },
                "required": ["type", "data"],
                "additionalProperties": false
              },
              {
                "type": "object",
                "properties": {
                  "type": {
                    "type": "string",
                    "const": "single_choice"
                  },
                  "data": {
                    "type": "object",
                    "properties": {
                      "options": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 64,
                        "items": {
                          "type": "object",
                          "properties": {
                            "optionIndex": {
                              "type": "integer",
                              "minimum": 0,
                              "maximum": 4294967295
                            },
                            "label": {
                              "type": "string",
                              "minLength": 1,
                              "maxLength": 256
                            }
                          },
                          "required": ["optionIndex", "label"],
                          "additionalProperties": false
                        }
                      }
                    },
                    "required": ["options"],
                    "additionalProperties": false
                  }
                },
                "required": ["type", "data"],
                "additionalProperties": false
              }
            ]
          }
        },
        "required": ["questionIndex", "prompt", "required", "input"],
        "additionalProperties": false
      }
    }
  },
  "required": ["questions"],
  "additionalProperties": false
}"#;

/// The exact frozen production definition/spec pair: the single source shared by the
/// standalone builtin ToolSet and the composed production ToolSet, so the disclosed
/// definition and spec are byte-identical in both selections.
pub(super) fn definition() -> ToolDefinition {
    ToolDefinition {
        spec: ToolSpec {
            name: ASK_USER_NAME
                .parse()
                .expect("the frozen ask_user ToolName is valid"),
            description: Arc::from(ASK_USER_DESCRIPTION),
            input_schema: ASK_USER_SCHEMA
                .parse()
                .expect("the frozen ask_user schema is valid"),
        },
        // UserQuestion exclusivity and call-index ordering are owned by the typed-plan
        // scheduler.  The definition itself does not impose Serial execution semantics on
        // unrelated ordinary operations in the composed production ToolSet.
        mode: ToolExecutionMode::Parallel,
    }
}

/// Builds the exact immutable production `ask_user` ToolSet: one definition, one matching
/// spec, the builtin planner, and the available empty sandbox contract.  `open` selects this
/// ToolSet once when the host opts in and passes it through the existing residency capture;
/// the default Runtime ToolSet stays empty.
pub(super) fn build_tool_set() -> Arc<ToolSet> {
    let definition = definition();
    let specs: Arc<[ToolSpec]> = Arc::from([definition.spec.clone()]);
    let definitions: Arc<[ToolDefinition]> = Arc::from([definition]);
    let planner: Arc<super::ToolPlanner> = Arc::new(plan);
    Arc::new(ToolSet {
        inner: Arc::new(ToolSetInner {
            definitions,
            specs,
            planner: Some(planner),
            sandbox: ToolSandboxContract::available([]),
        }),
    })
}

/// The synchronous pre-start plan for one exact `ask_user` call: a valid call plans the
/// typed UserQuestion (never an Execute/start shape), every parse or semantic failure plans
/// the frozen `PreExecution + Failed` result, and nothing else is ever produced.
/// `pub(super)` because the composed production ToolSet routes exactly this frozen planner.
pub(super) fn plan(request: ToolExecutionRequest) -> ToolExecutionPlan {
    let parsed = match parse_arguments(request.call().arguments()) {
        Ok(parsed) => parsed,
        Err(()) => return invalid_arguments_plan(),
    };
    let questions = match build_questions(parsed) {
        Ok(questions) => questions,
        Err(()) => return invalid_arguments_plan(),
    };
    let question = match UserQuestionRequest::reconstruct(questions.title, questions.fields) {
        Ok(question) => question,
        Err(_) => return invalid_arguments_plan(),
    };
    // The binding's closure owns the typed request; the plan carries its own clone so the
    // exact same typed question is both presented and validated by the move-only binding.
    let plan_request = question.clone();
    let answer = UserQuestionAnswerBinding {
        request,
        bind: Box::new(move |answer| bind_answer(&question, answer)),
    };
    ToolExecutionPlan::UserQuestion {
        request: plan_request,
        answer,
    }
}

fn invalid_arguments_plan() -> ToolExecutionPlan {
    ToolExecutionPlan::PreExecution(invalid_arguments_result())
}

fn invalid_arguments_result() -> ToolExecutionResult {
    ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Failed,
        content: ToolResultContent::from_text_parts(vec![INVALID_ARGUMENTS_TEXT.to_owned()])
            .expect("the frozen invalid-arguments text is a valid bounded part"),
    }
}

/// The move-only answer binding: re-validates the typed host answer through the exact
/// `UserQuestionRequest::validate_answer` owner check and projects a validated answer as
/// exactly one deterministic compact JSON Text part under `PreExecution + Succeeded`.
///
/// The host answer already passed the exact owner validation before it ever reached the
/// binding, so a re-validation mismatch here is an invariant that fails closed to
/// `Abandoned { RuntimeFailure }`; the same fail-closed reason covers any render-bound
/// failure, so the builtin can never emit malformed model-visible output.
fn bind_answer(question: &UserQuestionRequest, answer: UserQuestionAnswer) -> ToolExecutionResult {
    let validated = match question.validate_answer(answer) {
        Ok(validated) => validated,
        Err(_) => {
            return ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            };
        }
    };
    let content = match render_answer(&validated) {
        Ok(content) => content,
        Err(_) => {
            return ToolExecutionResult::Abandoned {
                reason: ToolAbandonReason::RuntimeFailure,
            };
        }
    };
    ToolExecutionResult::PreExecution {
        disposition: ToolResultDisposition::Succeeded,
        content,
    }
}

/// Renders the validated answer as one deterministic compact JSON Text part, bounded by the
/// existing `ToolResultContent` contract.  The projection envelope matches
/// `user_answer_encoded_len` exactly (same fixed literals, serde escaping never exceeds the
/// canonical escaping count), so a validated answer always fits the 65,536-byte part limit.
fn render_answer(answer: &UserQuestionAnswer) -> Result<ToolResultContent, ToolValueError> {
    let projection = AnswerProjection {
        answers: answer
            .answers()
            .iter()
            .map(|entry| AnswerProjectionEntry {
                question_index: entry.question_index(),
                value: match entry.value() {
                    UserQuestionAnswerValue::Text(text) => {
                        AnswerProjectionValue::Text(text.as_ref())
                    }
                    UserQuestionAnswerValue::Choice { option_index } => {
                        AnswerProjectionValue::Choice(AnswerProjectionChoice {
                            option_index: *option_index,
                        })
                    }
                },
            })
            .collect(),
    };
    let json = serde_json::to_string(&projection).map_err(|_| ToolValueError::InvalidQuestion)?;
    ToolResultContent::from_text_parts(vec![json])
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerProjection<'a> {
    answers: Vec<AnswerProjectionEntry<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerProjectionEntry<'a> {
    question_index: u32,
    value: AnswerProjectionValue<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "data")]
enum AnswerProjectionValue<'a> {
    Text(&'a str),
    Choice(AnswerProjectionChoice),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AnswerProjectionChoice {
    option_index: u32,
}

/// The strict private serde mirror of the closed arguments object: unknown fields are
/// rejected at every layer, and the semantic constructors stay the byte/count/index
/// authority.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserArguments {
    title: Option<String>,
    questions: Vec<AskUserQuestion>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserQuestion {
    question_index: u32,
    prompt: String,
    required: bool,
    input: AskUserInput,
}

/// The strict adjacent input object: exactly `type` and `data`, with the pairing checked
/// after the strict per-variant data parse.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserInput {
    #[serde(rename = "type")]
    kind: AskUserInputType,
    data: AskUserInputData,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum AskUserInputType {
    Text,
    SingleChoice,
}

/// The strict per-variant `data` parse: each variant's inner struct rejects unknown fields,
/// so a cross-variant or unknown shape matches no variant and fails the whole parse.
#[derive(Deserialize)]
#[serde(untagged)]
enum AskUserInputData {
    Text(AskUserTextData),
    SingleChoice(AskUserSingleChoiceData),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserTextData {
    multiline: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskUserSingleChoiceData {
    options: Vec<AskUserChoiceOption>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AskUserChoiceOption {
    option_index: u32,
    label: String,
}

fn parse_arguments(arguments: &BoundedJsonObject) -> Result<AskUserArguments, ()> {
    serde_json::from_str(arguments.canonical_json()).map_err(|_| ())
}

/// Builds the typed questions through the existing semantic constructors: every index,
/// count, text byte and safe-text rule is enforced there, never by the schema guidance.
fn build_questions(arguments: AskUserArguments) -> Result<BuiltQuestions, ()> {
    let fields = arguments
        .questions
        .into_iter()
        .map(|question| {
            let input = question.input.into_input()?;
            UserQuestionField::reconstruct(
                question.question_index,
                question.prompt,
                question.required,
                input,
            )
            .map_err(|_| ())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BuiltQuestions {
        title: arguments.title,
        fields,
    })
}

struct BuiltQuestions {
    title: Option<String>,
    fields: Vec<UserQuestionField>,
}

impl AskUserInput {
    fn into_input(self) -> Result<UserQuestionInput, ()> {
        match (self.kind, self.data) {
            (AskUserInputType::Text, AskUserInputData::Text(data)) => Ok(UserQuestionInput::Text {
                multiline: data.multiline,
            }),
            (AskUserInputType::SingleChoice, AskUserInputData::SingleChoice(data)) => {
                let options = data
                    .options
                    .into_iter()
                    .map(|option| {
                        UserQuestionChoice::reconstruct(option.option_index, option.label)
                            .map_err(|_| ())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(UserQuestionInput::SingleChoice {
                    options: options.into(),
                })
            }
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::tools::{
        ToolCall, ToolExecutionOutcome, ToolExecutionRequest, ToolOutcomeSource,
        ToolResultDisposition, ToolSet, ToolStartGate, UserQuestionAnswer, UserQuestionFieldAnswer,
        UserQuestionInput, UserQuestionRequest,
    };

    use super::*;

    const ITEM_ID: &str = "itm_00000000000000000000000000000001";

    fn set() -> Arc<ToolSet> {
        build_tool_set()
    }

    fn request_for(arguments: &str) -> ToolExecutionRequest {
        ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_ask".parse().unwrap(),
                ASK_USER_NAME.parse().unwrap(),
                arguments.parse().unwrap(),
                0,
            ),
        )
    }

    fn choice_arguments(option_count: u32) -> String {
        let options = (0..option_count)
            .map(|index| format!(r#"{{"optionIndex":{index},"label":"O{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"{{"questions":[{{"questionIndex":0,"prompt":"Pick","required":true,"input":{{"type":"single_choice","data":{{"options":[{options}]}}}}}}]}}"#
        )
    }

    /// Plans one call and destructures the exact typed plan; panics on any other shape.
    fn plan_user_question(
        set: &ToolSet,
        request: &ToolExecutionRequest,
    ) -> (UserQuestionRequest, UserQuestionAnswerBinding) {
        match set.plan(request) {
            Some(ToolExecutionPlan::UserQuestion { request, answer }) => (request, answer),
            _plan => panic!("expected a UserQuestion plan"),
        }
    }

    /// Plans one call and returns the frozen PreExecution result; panics on any other shape.
    fn plan_failure(set: &ToolSet, request: &ToolExecutionRequest) -> ToolExecutionResult {
        match set.plan(request) {
            Some(ToolExecutionPlan::PreExecution(result)) => result,
            _plan => panic!(
                "expected a PreExecution plan for arguments {}",
                request.call().arguments().canonical_json()
            ),
        }
    }

    #[test]
    fn builtin_defines_exactly_ask_user_with_the_frozen_description_and_closed_schema() {
        let set = set();
        let definitions = set.definitions();
        assert_eq!(definitions.len(), 1);
        let definition = &definitions[0];
        assert_eq!(definition.name().as_str(), ASK_USER_NAME);
        assert_eq!(definition.mode(), ToolExecutionMode::Parallel);
        assert_eq!(definition.spec.description.as_ref(), ASK_USER_DESCRIPTION);

        // The prompt view discloses exactly the same single spec (name, description,
        // closed schema); planner and sandbox internals never enter the model context.
        let view = set.prompt_view();
        assert!(!view.is_empty());
        assert_eq!(view.specs().len(), 1);
        assert_eq!(view.specs()[0].name().as_str(), ASK_USER_NAME);
        assert_eq!(view.specs()[0].description(), ASK_USER_DESCRIPTION);

        // The disclosed schema is exactly the frozen schema: canonical bytes round-trip to
        // the same semantic value and stay within the bounded schema limit.
        let schema = view.specs()[0].input_schema();
        assert_eq!(
            schema.canonical_json(),
            ASK_USER_SCHEMA
                .parse::<crate::wire::BoundedJsonSchema>()
                .unwrap()
                .canonical_json()
        );
        assert!(
            schema.canonical_bytes().len()
                <= crate::wire::ProtocolLimits::v1_0()
                    .embedded_json
                    .schema
                    .max_encoded_bytes as usize
        );

        // The canonical disclosure is a closed object: every layer rejects unknown fields,
        // and the count bounds mirror the authoritative semantic limits.
        let canonical: serde_json::Value =
            serde_json::from_str(schema.canonical_json()).expect("the schema is valid JSON");
        let root = canonical.as_object().expect("the schema root is an object");
        assert_eq!(
            root.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(
            root.get("required"),
            Some(&serde_json::json!(["questions"]))
        );
        let questions = root
            .get("properties")
            .and_then(|value| value.get("questions"))
            .expect("the schema discloses questions");
        assert_eq!(questions.get("minItems"), Some(&serde_json::json!(1)));
        assert_eq!(questions.get("maxItems"), Some(&serde_json::json!(32)));
        let question = questions
            .get("items")
            .expect("the schema discloses the question shape");
        assert_eq!(
            question.get("required"),
            Some(&serde_json::json!([
                "questionIndex",
                "prompt",
                "required",
                "input"
            ]))
        );
        assert_eq!(
            question.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
        let input = question
            .get("properties")
            .and_then(|value| value.get("input"))
            .expect("the schema discloses the input shape");
        let variants = input
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .expect("the input shape is an exact adjacent-union pair");
        assert_eq!(variants.len(), 2);
        for variant in variants {
            assert_eq!(
                variant.get("required"),
                Some(&serde_json::json!(["type", "data"]))
            );
            assert_eq!(
                variant.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false))
            );
        }
        assert_eq!(
            variants[0]
                .get("properties")
                .and_then(|value| value.get("type"))
                .and_then(|value| value.get("const")),
            Some(&serde_json::json!("text"))
        );
        let text_data = variants[0]
            .get("properties")
            .and_then(|value| value.get("data"))
            .expect("the text variant discloses its exact data shape");
        assert_eq!(
            text_data.get("required"),
            Some(&serde_json::json!(["multiline"]))
        );
        assert_eq!(
            variants[1]
                .get("properties")
                .and_then(|value| value.get("type"))
                .and_then(|value| value.get("const")),
            Some(&serde_json::json!("single_choice"))
        );
        let choice_data = variants[1]
            .get("properties")
            .and_then(|value| value.get("data"))
            .expect("the choice variant discloses its exact data shape");
        assert_eq!(
            choice_data.get("required"),
            Some(&serde_json::json!(["options"]))
        );
        let options = choice_data
            .get("properties")
            .and_then(|value| value.get("options"))
            .expect("the choice variant discloses options");
        assert_eq!(options.get("minItems"), Some(&serde_json::json!(1)));
        assert_eq!(options.get("maxItems"), Some(&serde_json::json!(64)));
        let option = options
            .get("items")
            .expect("the schema discloses the option shape");
        assert_eq!(
            option.get("required"),
            Some(&serde_json::json!(["optionIndex", "label"]))
        );
        assert_eq!(
            option.get("additionalProperties"),
            Some(&serde_json::Value::Bool(false))
        );
    }

    #[test]
    fn default_tool_set_stays_empty_and_the_opt_in_builtin_is_the_only_production_tool() {
        assert!(ToolSet::empty().definitions().is_empty());
        assert!(ToolSet::empty().prompt_view().is_empty());
        assert_eq!(set().definitions().len(), 1);
        assert_eq!(set().prompt_view().specs().len(), 1);
    }

    #[test]
    fn valid_text_choice_mixed_nullable_title_and_non_contiguous_indices_build_one_question_request()
     {
        let set = set();

        // Omitted and explicit-null titles both map to None; non-contiguous question and
        // option indices are valid as long as they are strictly increasing.
        for arguments in [
            r#"{"questions":[{"questionIndex":3,"prompt":"Where to deploy?","required":true,"input":{"type":"text","data":{"multiline":false}}}]}"#,
            r#"{"title":null,"questions":[{"questionIndex":3,"prompt":"Where to deploy?","required":true,"input":{"type":"text","data":{"multiline":false}}}]}"#,
        ] {
            let (request, _binding) = plan_user_question(&set, &request_for(arguments));
            assert_eq!(request.title(), None);
            assert_eq!(request.questions().len(), 1);
            let question = &request.questions()[0];
            assert_eq!(question.question_index(), 3);
            assert_eq!(question.prompt(), "Where to deploy?");
            assert!(question.required());
            assert!(matches!(
                question.input(),
                UserQuestionInput::Text { multiline: false }
            ));
        }

        // A nullable non-null title and a single-choice question with non-contiguous option
        // indices (1 and 3).
        let (request, _binding) = plan_user_question(
            &set,
            &request_for(
                r#"{"title":"Survey","questions":[{"questionIndex":7,"prompt":"Pick one","required":false,"input":{"type":"single_choice","data":{"options":[{"optionIndex":1,"label":"A"},{"optionIndex":3,"label":"B"}]}}}]}"#,
            ),
        );
        assert_eq!(request.title(), Some("Survey"));
        let question = &request.questions()[0];
        assert_eq!(question.question_index(), 7);
        assert!(!question.required());
        match question.input() {
            UserQuestionInput::SingleChoice { options } => {
                assert_eq!(options.len(), 2);
                assert_eq!(options[0].option_index(), 1);
                assert_eq!(options[0].label(), "A");
                assert_eq!(options[1].option_index(), 3);
                assert_eq!(options[1].label(), "B");
            }
            UserQuestionInput::Text { .. } => panic!("expected a single-choice input"),
        }

        // Mixed text/choice questions with non-contiguous question indices 1, 5, 9 and a
        // multiline text input.
        let (request, _binding) = plan_user_question(
            &set,
            &request_for(
                r#"{"questions":[{"questionIndex":1,"prompt":"Name","required":true,"input":{"type":"text","data":{"multiline":false}}},{"questionIndex":5,"prompt":"Why","required":false,"input":{"type":"single_choice","data":{"options":[{"optionIndex":0,"label":"Because"}]}}},{"questionIndex":9,"prompt":"Notes","required":false,"input":{"type":"text","data":{"multiline":true}}}]}"#,
            ),
        );
        assert_eq!(request.questions().len(), 3);
        assert_eq!(
            request
                .questions()
                .iter()
                .map(|question| question.question_index())
                .collect::<Vec<_>>(),
            [1, 5, 9]
        );
        assert!(matches!(
            request.questions()[2].input(),
            UserQuestionInput::Text { multiline: true }
        ));

        // The full 32-question bound is accepted (the owner constructor is the authority).
        let mut questions = String::from(r#"{"questions":["#);
        for index in 0..32 {
            if index != 0 {
                questions.push(',');
            }
            questions.push_str(&format!(
                r#"{{"questionIndex":{index},"prompt":"P{index}","required":false,"input":{{"type":"text","data":{{"multiline":false}}}}}}"#
            ));
        }
        questions.push_str("]}");
        let (request, _binding) = plan_user_question(&set, &request_for(&questions));
        assert_eq!(request.questions().len(), 32);

        // The full 64-option owner bound is accepted.
        let choices = choice_arguments(64);
        let (request, _binding) = plan_user_question(&set, &request_for(&choices));
        match request.questions()[0].input() {
            UserQuestionInput::SingleChoice { options } => assert_eq!(options.len(), 64),
            UserQuestionInput::Text { .. } => panic!("expected a single-choice input"),
        }
    }

    #[test]
    fn parse_and_semantic_failures_settle_the_frozen_preexecution_failed_result_without_interaction()
     {
        let set = set();
        let text = r#"{"type":"text","data":{"multiline":false}}"#;
        let choice =
            r#"{"type":"single_choice","data":{"options":[{"optionIndex":0,"label":"A"}]}}"#;
        let question = |index: u32, input: &str| {
            format!(r#"{{"questionIndex":{index},"prompt":"P","required":true,"input":{input}}}"#)
        };
        let mut invalid = vec![
            // Structural parse failures at every layer.
            "{}".to_owned(),
            r#"{"questions":[]}"#.to_owned(),
            format!(r#"{{"questions":[{}],"extra":1}}"#, question(0, text)),
            format!(r#"{{"questions":[{{"questionIndex":0,"required":true,"input":{text}}}]}}"#),
            format!(r#"{{"questions":[{{"questionIndex":0,"prompt":"P","input":{text}}}]}}"#),
            format!(r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true}}]}}"#),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{text},"extra":1}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"text","data":{{"multiline":false}},"extra":1}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"data":{{}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"checkbox","data":{{}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"text","data":{{}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"text","data":{{"multiline":false,"extra":true}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[]}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":0}}]}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":0,"label":"A","extra":1}}]}}}}}}]}}"#
            ),
            // Cross-variant data shapes never match the strict adjacent mirror.
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"text","data":{{"options":[{{"optionIndex":0,"label":"A"}}]}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"multiline":true}}}}}}]}}"#
            ),
            // Index semantics: out-of-range, negative, fractional, duplicated, and the
            // 33-question count beyond the authoritative limit.
            format!(
                r#"{{"questions":[{{"questionIndex":{},"prompt":"P","required":true,"input":{text}}}]}}"#,
                "4294967296"
            ),
            format!(
                r#"{{"questions":[{}]}}"#,
                question(0, text).replace("\"questionIndex\":0", "\"questionIndex\":-1")
            ),
            format!(
                r#"{{"questions":[{}]}}"#,
                question(0, text).replace("\"questionIndex\":0", "\"questionIndex\":1.5")
            ),
            format!(
                r#"{{"questions":[{},{}]}}"#,
                question(1, text),
                question(1, text)
            ),
            format!(
                r#"{{"questions":[{},{}]}}"#,
                question(2, text),
                question(1, text)
            ),
            format!(
                r#"{{"questions":[{}]}}"#,
                question(0, choice).replace("\"optionIndex\":0", "\"optionIndex\":-1")
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":0,"label":"A"}},{{"optionIndex":0,"label":"B"}}]}}}}}}]}}"#
            ),
            format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":1,"label":"A"}},{{"optionIndex":0,"label":"B"}}]}}}}}}]}}"#
            ),
        ];

        // The 33-question count exceeds `max_interaction_questions` (32): the owner
        // constructor is the authoritative count gate.
        let mut thirty_three = String::from(r#"{"questions":["#);
        for index in 0..33 {
            if index != 0 {
                thirty_three.push(',');
            }
            thirty_three.push_str(&format!(
                r#"{{"questionIndex":{index},"prompt":"P","required":false,"input":{{"type":"text","data":{{"multiline":false}}}}}}"#
            ));
        }
        thirty_three.push_str("]}");
        invalid.push(thirty_three);

        // The 65-option count exceeds `max_choices_per_question` (64).
        invalid.push(choice_arguments(65));

        // Byte and text semantics: the 8,192-byte prompt boundary with multi-byte UTF-8, an
        // overlong prompt, an unsafe control character, an overlong option label, and an
        // overlong title.
        let utf8_prompt = "é".repeat(4_096);
        assert_eq!(utf8_prompt.len(), 8_192);
        invalid.push(format!(
            r#"{{"questions":[{{"questionIndex":0,"prompt":"{}","required":true,"input":{text}}}]}}"#,
            "x".repeat(8_193)
        ));
        invalid.push(format!(
            r#"{{"questions":[{{"questionIndex":0,"prompt":"bad\u0001","required":true,"input":{text}}}]}}"#
        ));
        invalid.push(format!(
            r#"{{"questions":[{{"questionIndex":0,"prompt":"P","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":0,"label":"{}"}}]}}}}}}]}}"#,
            "x".repeat(257)
        ));
        invalid.push(format!(
            r#"{{"title":"{}","questions":[{}]}}"#,
            "x".repeat(257),
            question(0, text)
        ));

        for (index, arguments) in invalid.iter().enumerate() {
            let request = request_for(arguments);
            let result = plan_failure(&set, &request);
            assert_eq!(
                result,
                ToolExecutionResult::PreExecution {
                    disposition: ToolResultDisposition::Failed,
                    content: ToolResultContent::from_text_parts(vec![
                        INVALID_ARGUMENTS_TEXT.to_owned()
                    ])
                    .unwrap(),
                },
                "arguments #{index} {arguments:?} must settle the frozen failed pre-execution result"
            );
        }

        // The 8,192-byte prompt itself is accepted when it stays within the boundary.
        let (request, _binding) = plan_user_question(
            &set,
            &request_for(&format!(
                r#"{{"questions":[{{"questionIndex":0,"prompt":"{utf8_prompt}","required":true,"input":{text}}}]}}"#
            )),
        );
        assert_eq!(request.questions()[0].prompt(), utf8_prompt);
    }

    #[test]
    fn answers_bind_through_the_exact_owner_validation_to_one_deterministic_text_part() {
        let set = set();
        let request = request_for(
            r#"{"questions":[{"questionIndex":3,"prompt":"Message","required":true,"input":{"type":"text","data":{"multiline":false}}},{"questionIndex":7,"prompt":"Pick","required":false,"input":{"type":"single_choice","data":{"options":[{"optionIndex":11,"label":"A"},{"optionIndex":13,"label":"B"}]}}}]}"#,
        );
        let (question, binding) = plan_user_question(&set, &request);
        assert!(
            question
                .validate_answer(
                    UserQuestionAnswer::new(vec![
                        UserQuestionFieldAnswer::text(3, "hello").unwrap(),
                        UserQuestionFieldAnswer::choice(7, 11),
                    ])
                    .unwrap()
                )
                .is_ok()
        );

        let outcome = binding.bind(
            &request,
            UserQuestionAnswer::new(vec![
                UserQuestionFieldAnswer::text(3, "hello").unwrap(),
                UserQuestionFieldAnswer::choice(7, 11),
            ])
            .unwrap(),
        );
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                item_id,
                tool_call_id,
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
            } if item_id == request.item_id()
                && tool_call_id == *request.call().tool_call_id()
                && content.parts().len() == 1
                && content.parts()[0].as_text()
                    == r#"{"answers":[{"questionIndex":3,"value":{"type":"text","data":"hello"}},{"questionIndex":7,"value":{"type":"choice","data":{"optionIndex":11}}}]}"#
        ));
    }

    #[test]
    fn answer_rendering_is_deterministic_and_escapes_canonically_without_silent_normalization() {
        let set = set();
        let request = request_for(
            r#"{"questions":[{"questionIndex":3,"prompt":"Message","required":true,"input":{"type":"text","data":{"multiline":true}}}]}"#,
        );
        let (question, binding) = plan_user_question(&set, &request);

        // CRLF is normalized by the existing owner constructor (never by the renderer), and
        // quotes/backslashes are escaped canonically by the projection.
        let answer = UserQuestionAnswer::new(vec![
            UserQuestionFieldAnswer::text(3, "say \"hi\" \\ done\r\nnext").unwrap(),
        ])
        .unwrap();
        assert!(question.validate_answer(answer.clone()).is_ok());
        let outcome = binding.bind(&request, answer);
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text()
                    == r#"{"answers":[{"questionIndex":3,"value":{"type":"text","data":"say \"hi\" \\ done\nnext"}}]}"#
        ));

        // The same validated answer always renders the exact same compact JSON: a second
        // fresh plan produces a byte-identical part.
        let (_, second_binding) = plan_user_question(&set, &request);
        let answer = UserQuestionAnswer::new(vec![
            UserQuestionFieldAnswer::text(3, "say \"hi\" \\ done\r\nnext").unwrap(),
        ])
        .unwrap();
        let second = match second_binding.bind(&request, answer) {
            ToolExecutionOutcome::Completed { content, .. } => content,
            _ => panic!("the second binding succeeds"),
        };
        assert_eq!(outcome_content(&outcome), &second);
    }

    fn outcome_content(outcome: &ToolExecutionOutcome) -> &ToolResultContent {
        match outcome {
            ToolExecutionOutcome::Completed { content, .. } => content,
            _ => panic!("expected a Completed outcome"),
        }
    }

    #[test]
    fn optional_unanswered_questions_render_an_empty_answers_array_in_ascending_order() {
        let set = set();
        let request = request_for(
            r#"{"questions":[{"questionIndex":1,"prompt":"Name","required":true,"input":{"type":"text","data":{"multiline":false}}},{"questionIndex":5,"prompt":"Why","required":false,"input":{"type":"text","data":{"multiline":false}}},{"questionIndex":9,"prompt":"Notes","required":false,"input":{"type":"text","data":{"multiline":false}}}]}"#,
        );
        let (_question, binding) = plan_user_question(&set, &request);

        // The optional question 5 is unanswered; the answers keep ascending order 1, 9.
        let outcome = binding.bind(
            &request,
            UserQuestionAnswer::new(vec![
                UserQuestionFieldAnswer::text(1, "hello").unwrap(),
                UserQuestionFieldAnswer::text(9, "world").unwrap(),
            ])
            .unwrap(),
        );
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text()
                    == r#"{"answers":[{"questionIndex":1,"value":{"type":"text","data":"hello"}},{"questionIndex":9,"value":{"type":"text","data":"world"}}]}"#
        ));

        // A fully optional question with no answers renders the empty array.
        let empty_request = request_for(
            r#"{"questions":[{"questionIndex":0,"prompt":"Optional","required":false,"input":{"type":"text","data":{"multiline":false}}}]}"#,
        );
        let (empty_question, empty_binding) = plan_user_question(&set, &empty_request);
        let empty = UserQuestionAnswer::new(Vec::new()).unwrap();
        assert!(empty_question.validate_answer(empty.clone()).is_ok());
        let outcome = empty_binding.bind(&empty_request, empty);
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                ref content,
                ..
            } if content.parts().len() == 1
                && content.parts()[0].as_text() == r#"{"answers":[]}"#
        ));
    }

    #[test]
    fn answer_validation_invariants_fail_closed_to_abandoned_runtime_failure() {
        let set = set();
        let arguments = r#"{"questions":[{"questionIndex":3,"prompt":"Message","required":true,"input":{"type":"text","data":{"multiline":false}}},{"questionIndex":7,"prompt":"Pick","required":false,"input":{"type":"single_choice","data":{"options":[{"optionIndex":11,"label":"A"}]}}}]}"#;
        let request = request_for(arguments);
        let (question, _binding) = plan_user_question(&set, &request);
        let invalid_answers = [
            // Missing required question.
            UserQuestionAnswer::new(Vec::new()).unwrap(),
            // Unknown question index.
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(5, "x").unwrap()]).unwrap(),
            // Wrong value family for the text question.
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::choice(3, 11)]).unwrap(),
            // Unknown choice index for the single-choice question.
            UserQuestionAnswer::new(vec![
                UserQuestionFieldAnswer::text(3, "x").unwrap(),
                UserQuestionFieldAnswer::choice(7, 12),
            ])
            .unwrap(),
            // Empty required text.
            UserQuestionAnswer::new(vec![
                UserQuestionFieldAnswer::text(3, "").unwrap(),
                UserQuestionFieldAnswer::choice(7, 11),
            ])
            .unwrap(),
        ];
        for answer in invalid_answers {
            assert!(question.validate_answer(answer.clone()).is_err());
            // Each bind consumes a fresh move-only binding from a fresh plan.
            let (_, binding) = plan_user_question(&set, &request);
            let outcome = binding.bind(&request, answer);
            assert!(matches!(
                outcome,
                ToolExecutionOutcome::Abandoned {
                    item_id,
                    tool_call_id,
                    reason: crate::tools::ToolAbandonReason::RuntimeFailure,
                } if item_id == request.item_id()
                    && tool_call_id == *request.call().tool_call_id()
            ));
        }
    }

    #[test]
    fn builtin_never_produces_an_execute_plan_or_touches_a_start_gate() {
        let set = set();
        let valid = request_for(
            r#"{"questions":[{"questionIndex":0,"prompt":"Continue?","required":true,"input":{"type":"text","data":{"multiline":false}}}]}"#,
        );
        let invalid = request_for(r#"{"questions":[]}"#);

        // The plan is either a UserQuestion or a frozen PreExecution, never Execute, and
        // carries no start factory of any kind.
        for (request, expected) in [(&valid, true), (&invalid, false)] {
            match set.plan(request) {
                Some(ToolExecutionPlan::UserQuestion { .. }) => assert!(expected),
                Some(ToolExecutionPlan::PreExecution(result)) => {
                    assert!(!expected);
                    assert!(matches!(
                        result,
                        ToolExecutionResult::PreExecution {
                            disposition: ToolResultDisposition::Failed,
                            ..
                        }
                    ));
                }
                _plan => panic!("unexpected plan shape"),
            }
        }

        // Planning never reserves the exact request's start gate: the gate still accepts its
        // single reservation and start exactly like a never-touched gate.
        let gate = ToolStartGate::new(valid.clone());
        assert!(gate.reserve(&valid).unwrap().start().is_ok());

        // The answer outcome is always PreExecution (the question settles before any
        // execution), and an unknown tool name still plans to the unavailable path.
        let (question, binding) = match set.plan(&valid) {
            Some(ToolExecutionPlan::UserQuestion { request, answer }) => (request, answer),
            _ => panic!("the valid call plans a UserQuestion"),
        };
        let _ = question;
        let outcome = binding.bind(
            &valid,
            UserQuestionAnswer::new(vec![UserQuestionFieldAnswer::text(0, "yes").unwrap()])
                .unwrap(),
        );
        assert!(matches!(
            outcome,
            ToolExecutionOutcome::Completed {
                source: ToolOutcomeSource::PreExecution,
                disposition: ToolResultDisposition::Succeeded,
                ..
            }
        ));
        let unknown = ToolExecutionRequest::new(
            ITEM_ID.parse().unwrap(),
            ToolCall::new(
                "call_unknown".parse().unwrap(),
                "missing".parse().unwrap(),
                "{}".parse().unwrap(),
                0,
            ),
        );
        assert!(set.plan(&unknown).is_none());
    }

    #[test]
    fn question_request_construction_reuses_the_existing_owner_size_gates() {
        let set = set();

        // The per-field byte limits are enforced by the owner constructors: a title,
        // prompt, and label at their exact maxima build a valid request.
        let title = "t".repeat(256);
        let prompt = "x".repeat(8_192);
        let label = "x".repeat(256);
        let within = format!(
            r#"{{"title":"{title}","questions":[{{"questionIndex":0,"prompt":"{prompt}","required":true,"input":{{"type":"single_choice","data":{{"options":[{{"optionIndex":0,"label":"{label}"}}]}}}}}}]}}"#
        );
        let (request, _binding) = plan_user_question(&set, &request_for(&within));
        assert_eq!(request.title(), Some(title.as_str()));
        assert_eq!(request.questions()[0].prompt(), prompt);
        assert_eq!(request.questions()[0].prompt().len(), 8_192);

        // The aggregate interaction-view byte gate (131,072) is the owner's, not the
        // schema's: it is unreachable through a ToolCall (the arguments object itself is
        // already bounded at 65,536 bytes), so the owner constructor is the only place it
        // can fire.  Exercising it directly documents the exact owner boundary.
        let fields = (0..32)
            .map(|index| {
                UserQuestionField::reconstruct(
                    index,
                    "x".repeat(8_192),
                    false,
                    UserQuestionInput::Text { multiline: false },
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            UserQuestionRequest::reconstruct(Some(title), fields),
            Err(ToolValueError::InvalidQuestion)
        ));
    }

    #[test]
    fn production_answer_renderer_accepts_the_exact_owner_byte_boundary() {
        let maximum = crate::wire::ProtocolLimits::v1_0()
            .interaction
            .max_interaction_answer_bytes as usize;
        let empty = UserQuestionAnswer::new(
            (0..4)
                .map(|index| UserQuestionFieldAnswer::text(index, "").unwrap())
                .collect(),
        )
        .unwrap();
        let mut remaining = maximum - crate::tools::user_answer_encoded_len(&empty).unwrap();
        let answers = (0..4)
            .map(|index| {
                let size = remaining.min(16_384);
                remaining -= size;
                UserQuestionFieldAnswer::text(index, "x".repeat(size)).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(remaining, 0);
        let boundary = UserQuestionAnswer::new(answers).unwrap();
        assert_eq!(
            crate::tools::user_answer_encoded_len(&boundary),
            Some(maximum)
        );

        let rendered = render_answer(&boundary).expect("the exact owner boundary renders");
        assert_eq!(rendered.parts().len(), 1);
        assert_eq!(rendered.parts()[0].as_text().len(), maximum);
    }
}
