#![cfg(test)]
//! Controlled predictions over the existing compatible decision wire; no grants.
use anyhow::Context;
const DECISION_MODEL: &str = "prx-native-decision";

pub(super) fn answers(body: &serde_json::Value, action: &str, target: &str) -> anyhow::Result<serde_json::Value> {
    use serde_json::{Value, json};
    assert_eq!(
        body.get("model").and_then(Value::as_str),
        Some(DECISION_MODEL),
        "test: configured decision fixture model required"
    );
    let candidates = body
        .pointer("/state/candidates")
        .and_then(Value::as_array)
        .context("actual candidate projection")?;
    let target_index = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.get("action").and_then(Value::as_str) == Some(action)
                && candidate.get("target").and_then(Value::as_str) == Some(target)
        })
        .min_by_key(|(_, candidate)| candidate.get("exact_mention").and_then(Value::as_bool) != Some(true))
        .map(|(index, _)| index)
        .context("actual requested action/family candidate")?;
    let target_path = format!("$.candidates[{target_index}]");
    let questions = body
        .get("questions")
        .and_then(Value::as_object)
        .context("actual salted wire questions")?;
    anyhow::ensure!(!questions.is_empty(), "empty decision request");
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let instruction = question
            .get("instructions")
            .and_then(Value::as_str)
            .context("actual question instructions")?;
        let kind = question
            .get("type")
            .and_then(Value::as_str)
            .context("actual question type")?;
        let target_question = instruction.contains(&target_path);
        let candidate_question = candidates.iter().enumerate().any(|(index, _)| {
            let path = format!("$.candidates[{index}]");
            instruction == format!("Does {path} apply to this state?")
                || instruction == format!("Would {path} succeed?")
                || instruction == format!("Rate the risk of {path}.")
                || instruction == format!("Rate the task gain from {path}.")
        });
        let answer = match kind {
            "choice" => {
                let chosen = match instruction {
                    "Choose the best legal next action." => action,
                    "Choose the capability family needed by the next action." => target,
                    _ => anyhow::bail!("unexpected choice question"),
                };
                let options = question
                    .get("criteria")
                    .and_then(Value::as_object)
                    .context("choice criteria")?;
                anyhow::ensure!(options.contains_key(chosen), "required actual choice absent");
                let probabilities: serde_json::Map<String, Value> = options
                    .keys()
                    .map(|option| (option.clone(), json!(if option == chosen { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"choice","choice":chosen,"probabilities":probabilities,"confidence":1.0})
            }
            "noul" => {
                let yes = match instruction {
                    "Is the observed evidence sufficient for the next action?" => true,
                    "Could the observed artifacts be stale?" => false,
                    _ if candidate_question
                        && (instruction.starts_with("Does ") || instruction.starts_with("Would ")) =>
                    {
                        target_question
                    }
                    _ => anyhow::bail!("unexpected noul question"),
                };
                json!({"type":"noul","noul":if yes {1.0} else {0.0}})
            }
            "score" => {
                let high = match instruction {
                    "Rate the expected task gain." => true,
                    "Rate the risk of the next action." => false,
                    _ if candidate_question && instruction.starts_with("Rate the risk of ") => false,
                    _ if candidate_question && instruction.starts_with("Rate the task gain from ") => target_question,
                    _ => anyhow::bail!("unexpected score question"),
                };
                let levels = question
                    .get("criteria")
                    .and_then(Value::as_array)
                    .context("score criteria")?;
                let last = levels.len().checked_sub(1).context("nonempty score legend")?;
                let selected = if high { last } else { 0 };
                let legend: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, level)| (index.to_string(), level.clone()))
                    .collect();
                let probabilities: serde_json::Map<String, Value> = levels
                    .iter()
                    .enumerate()
                    .map(|(index, _)| (index.to_string(), json!(if index == selected { 1.0 } else { 0.0 })))
                    .collect();
                json!({"type":"score","score":selected,"legend":legend,"probabilities":probabilities,"confidence":1.0})
            }
            _ => anyhow::bail!("unexpected decision primitive"),
        };
        answers.insert(id.clone(), answer);
    }
    Ok(json!({"model":DECISION_MODEL,"answers":answers,"usage":{"input_tokens":20,"output_tokens":10}}))
}
