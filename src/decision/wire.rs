//! The System One wire codec (CDP-00B `DIFFS.md`, D-042), shared by every HTTP
//! decision backend.
//!
//! Encoding writes exactly the three wire fields `model`, `questions` and
//! `state`, as canonical JSON (object keys in byte order). PRX's
//! `protocol_version`, `request_id` and manifest are never sent.
//!
//! Every question id is salted (D-042 C-2): the wire id of question `q` is
//! `q-` followed by the first eight bytes (64 bits, D-057 L-4), in hex, of
//! `HMAC(DecisionKey, WireSalt, request_fingerprint ‖ q)`. The model never sees
//! question ids, so salting costs nothing in answer quality, and a response
//! replayed from any request with a different fingerprint names no question of
//! this one: it fails as [`ValidationRule::AnswerExtra`]. Option names stay
//! readable because the model reasons over them.
//!
//! Decoding rejects a key that appears twice in any JSON object PRX
//! interprets, a non-finite or out-of-range number, and every shape the
//! documented wire format does not allow, including the positional (JSON
//! array) form of the response and answer objects (D-057 M-3). A repeated
//! known field (`model`, `type`, ...) is `Malformed`; any other repeated key
//! is `AnswerDuplicated`. Fields the codec does not know, such as
//! `explanation` or `reasoning`, are skipped by the parser without being
//! materialized (AG-15). Usage is accounting only: a malformed usage value
//! is ignored with a warning instead of voiding the decision (D-057 L-8).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use sha2::{Digest as _, Sha256};

use super::error::{DecisionError, RequestRule, ValidationRule};
use super::key::{DecisionKey, KeyDomain};
use super::protocol::{
    ChoiceAnswer, DecisionAnswer, DecisionQuestion, DecisionRequest, DecisionResponse, DecisionUsage, NoulAnswer,
    Primitive, Probability, QuestionId, ScoreAnswer, StructuredLimits, StructuredValue, write_canonical,
    write_json_string, write_question,
};

/// The wire ids of `ids`, in the order given, salted with `key` over
/// `request_fingerprint`. Two ids that salt to the same wire id are refused
/// ([`RequestRule::WireIdCollision`]) before any transport exists.
pub(super) fn salted_question_ids<'q>(
    key: &DecisionKey,
    request_fingerprint: &[u8; 32],
    ids: impl Iterator<Item = &'q QuestionId>,
) -> Result<Vec<String>, DecisionError> {
    let mut seen = BTreeSet::new();
    let mut salted = Vec::new();
    for id in ids {
        let mac = key.hmac(KeyDomain::WireSalt, &[request_fingerprint, id.as_str().as_bytes()]);
        let wire_id = format!("q-{}", hex::encode(&mac[..8]));
        if !seen.insert(wire_id.clone()) {
            return Err(DecisionError::InvalidRequest(RequestRule::WireIdCollision));
        }
        salted.push(wire_id);
    }
    Ok(salted)
}

/// What the transport knows about a response besides its body.
#[derive(Debug, Clone, Copy)]
pub struct ResponseEnvelope<'a> {
    /// The adapter's own name: the wire has no backend field (DIFFS D-13).
    pub backend: &'a str,
    /// The provider request id from the response header, when present.
    pub provider_request_id: Option<&'a str>,
    /// Time from send to the complete body.
    pub elapsed_ms: u64,
}

/// The codec for one sealed request.
pub struct SystemOneCodec<'r> {
    request: &'r DecisionRequest,
    /// The request's questions with their wire ids, in id order.
    questions: Vec<(&'r QuestionId, &'r DecisionQuestion, String)>,
    /// Wire id to index into `questions`.
    by_wire_id: BTreeMap<String, usize>,
}

impl fmt::Debug for SystemOneCodec<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SystemOneCodec")
            .field("request_id", &self.request.request_id())
            .field("questions", &self.questions.len())
            .finish_non_exhaustive()
    }
}

impl<'r> SystemOneCodec<'r> {
    /// The codec for `request`, salting its question ids with `key`.
    pub fn new(request: &'r DecisionRequest, key: &DecisionKey) -> Result<Self, DecisionError> {
        let wire_ids = salted_question_ids(
            key,
            request.manifest().request_fingerprint(),
            request.questions().keys(),
        )?;
        let questions: Vec<_> = request
            .questions()
            .iter()
            .zip(wire_ids)
            .map(|((id, question), wire_id)| (id, question, wire_id))
            .collect();
        let by_wire_id = questions
            .iter()
            .enumerate()
            .map(|(index, (_, _, wire_id))| (wire_id.clone(), index))
            .collect();
        Ok(Self {
            request,
            questions,
            by_wire_id,
        })
    }

    /// The request body: `{"model":…,"questions":{…},"state":…}` in canonical
    /// JSON. The wire requires a model ([`RequestRule::ModelRequired`]).
    pub fn encode_request(&self) -> Result<Vec<u8>, DecisionError> {
        let model = self
            .request
            .model()
            .ok_or(DecisionError::InvalidRequest(RequestRule::ModelRequired))?;
        let mut ordered: Vec<(&str, &DecisionQuestion)> = self
            .questions
            .iter()
            .map(|(_, question, wire_id)| (wire_id.as_str(), *question))
            .collect();
        ordered.sort_unstable_by(|left, right| left.0.cmp(right.0));
        let mut out = String::from("{\"model\":");
        write_json_string(model, &mut out);
        out.push_str(",\"questions\":{");
        for (index, (wire_id, question)) in ordered.into_iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            write_json_string(wire_id, &mut out);
            out.push(':');
            write_question(question, &mut out);
        }
        out.push_str("},\"state\":");
        write_canonical(self.request.state().as_json(), &mut out);
        out.push('}');
        Ok(out.into_bytes())
    }

    /// Decode a response body into typed answers keyed by the request's
    /// question ids.
    ///
    /// Structural failures reject the whole body: undecodable JSON or shape
    /// ([`ValidationRule::Malformed`]), a repeated key
    /// ([`ValidationRule::AnswerDuplicated`]), an unknown wire id
    /// ([`ValidationRule::AnswerExtra`]), a primitive other than the question's
    /// ([`ValidationRule::WrongPrimitive`]), an option outside the criteria
    /// ([`ValidationRule::UnknownOption`]), a probability outside `[0, 1]`
    /// ([`ValidationRule::ProbabilityRange`]) and Score keys that are not the
    /// level indices ([`ValidationRule::LegendMismatch`]). The remaining
    /// invariants are the validator's.
    pub fn decode_response(
        &self,
        body: &[u8],
        envelope: ResponseEnvelope<'_>,
    ) -> Result<DecisionResponse, DecisionError> {
        let response_sha256: [u8; 32] = Sha256::digest(body).into();
        // A body past 4 GiB saturates, which the size rule still rejects.
        let raw_bytes = u32::try_from(body.len()).unwrap_or(u32::MAX);
        let MapOnly(wire) =
            serde_json::from_slice::<MapOnly<WireResponse>>(body).map_err(|_| invalid(ValidationRule::Malformed))?;
        if wire.answers.duplicated {
            return Err(invalid(ValidationRule::AnswerDuplicated));
        }
        let mut answers = BTreeMap::new();
        let limits = self.request.structured_limits();
        for (wire_id, MapOnly(answer)) in wire.answers.entries {
            let (id, question, _) = self
                .by_wire_id
                .get(&wire_id)
                .and_then(|&index| self.questions.get(index))
                .ok_or_else(|| invalid(ValidationRule::AnswerExtra))?;
            answers.insert((*id).clone(), decode_answer(question, answer, limits)?);
        }
        let usage = wire.usage.unwrap_or_default();
        if usage.malformed {
            // Accounting only (DIFFS D-14): the decision stands, the counts
            // that could not be read are left out.
            tracing::warn!(
                rule = "usage_malformed",
                request_id = %self.request.request_id(),
                "decision response usage is malformed and was ignored"
            );
        }
        let usage = DecisionUsage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost_micro_usd: None,
        };
        Ok(DecisionResponse {
            backend: envelope.backend.to_string(),
            resolved_model: wire.model.unwrap_or_default(),
            answers,
            usage,
            provider_request_id: envelope.provider_request_id.map(str::to_string),
            elapsed_ms: envelope.elapsed_ms,
            raw_bytes,
            response_sha256,
        })
    }
}

const fn invalid(rule: ValidationRule) -> DecisionError {
    DecisionError::InvalidResponse(rule)
}

fn decode_answer(
    question: &DecisionQuestion,
    answer: WireAnswer,
    limits: &StructuredLimits,
) -> Result<DecisionAnswer, DecisionError> {
    let primitive = match answer.kind.as_str() {
        "choice" => Primitive::Choice,
        "score" => Primitive::Score,
        "noul" => Primitive::Noul,
        _ => return Err(invalid(ValidationRule::WrongPrimitive)),
    };
    if primitive != question.primitive() {
        return Err(invalid(ValidationRule::WrongPrimitive));
    }
    let required = |value: Option<f64>| value.ok_or_else(|| invalid(ValidationRule::Malformed));
    match question {
        DecisionQuestion::Choice { criteria, .. } => {
            let chosen = answer.choice.ok_or_else(|| invalid(ValidationRule::Malformed))?;
            let probabilities = answer.probabilities.ok_or_else(|| invalid(ValidationRule::Malformed))?;
            let confidence = required(answer.confidence)?;
            if probabilities.duplicated {
                return Err(invalid(ValidationRule::AnswerDuplicated));
            }
            let option = |name: &str| {
                criteria
                    .get_key_value(name)
                    .map(|(option, _)| option.clone())
                    .ok_or_else(|| invalid(ValidationRule::UnknownOption))
            };
            let chosen = option(&chosen)?;
            let mut distribution = BTreeMap::new();
            for (name, probability) in probabilities.entries {
                distribution.insert(option(&name)?, Probability::new(probability)?);
            }
            Ok(DecisionAnswer::Choice(ChoiceAnswer {
                chosen,
                probabilities: distribution,
                confidence: Probability::new(confidence)?,
            }))
        }
        DecisionQuestion::Score { .. } => {
            let expected = required(answer.score)?;
            let probabilities = answer.probabilities.ok_or_else(|| invalid(ValidationRule::Malformed))?;
            let legend = answer.legend.ok_or_else(|| invalid(ValidationRule::Malformed))?;
            let confidence = required(answer.confidence)?;
            let nested_duplicate = legend.entries.values().any(|level| level.duplicated);
            if probabilities.duplicated || legend.duplicated || nested_duplicate {
                return Err(invalid(ValidationRule::AnswerDuplicated));
            }
            let distribution = by_level_index(probabilities.entries)?
                .into_iter()
                .map(Probability::new)
                .collect::<Result<Vec<_>, _>>()?;
            let legend = by_level_index(legend.entries)?
                .into_iter()
                .map(|level| {
                    StructuredValue::with_limits(level.value, limits)
                        .map_err(|_| invalid(ValidationRule::LegendMismatch))
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(DecisionAnswer::Score(ScoreAnswer {
                distribution,
                expected,
                legend,
                confidence: Probability::new(confidence)?,
            }))
        }
        DecisionQuestion::Noul { .. } => Ok(DecisionAnswer::Noul(NoulAnswer {
            probability_true: Probability::new(required(answer.noul)?)?,
        })),
    }
}

/// The values of a map keyed by the level indices `"0"`..`"n-1"` (canonical
/// decimal), in index order. Any other key set is a legend mismatch.
fn by_level_index<V>(entries: BTreeMap<String, V>) -> Result<Vec<V>, DecisionError> {
    let mut indexed = BTreeMap::new();
    for (key, value) in entries {
        let index = key
            .parse::<usize>()
            .ok()
            .filter(|index| index.to_string() == key)
            .ok_or_else(|| invalid(ValidationRule::LegendMismatch))?;
        indexed.insert(index, value);
    }
    if indexed.keys().copied().eq(0..indexed.len()) {
        Ok(indexed.into_values().collect())
    } else {
        Err(invalid(ValidationRule::LegendMismatch))
    }
}

// ---------------------------------------------------------------------------
// Wire structures
// ---------------------------------------------------------------------------

/// The response envelope. Unknown fields are skipped without being read into
/// a value. Decoded only through [`MapOnly`].
#[derive(Deserialize)]
struct WireResponse {
    /// Optional here so that an absent model surfaces as the validator's
    /// `IdentityMissing` rather than as a decode failure.
    model: Option<String>,
    answers: UniqueMap<MapOnly<WireAnswer>>,
    usage: Option<LenientUsage>,
}

/// A value that must be a JSON object. A derived struct deserializer also
/// accepts the positional (array) form; this wrapper asks the parser for a
/// map only, so an array, string or number is refused (D-057 M-3).
struct MapOnly<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for MapOnly<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapOnlyVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for MapOnlyVisitor<T> {
            type Value = MapOnly<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                T::deserialize(de::value::MapAccessDeserializer::new(map)).map(MapOnly)
            }
        }

        deserializer.deserialize_map(MapOnlyVisitor(PhantomData))
    }
}

/// Usage accounting, read leniently (D-057 L-8): a count that is not an
/// integer in `u32` range, a repeated count, or a `usage` value that is not
/// an object is dropped and flagged instead of rejecting the response.
#[derive(Default)]
struct LenientUsage {
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    malformed: bool,
}

impl<'de> Deserialize<'de> for LenientUsage {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UsageVisitor;

        impl<'de> Visitor<'de> for UsageVisitor {
            type Value = LenientUsage;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a usage object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut usage = LenientUsage::default();
                let (mut input_seen, mut output_seen) = (false, false);
                while let Some(key) = map.next_key::<UsageKey>()? {
                    let (slot, seen) = match key {
                        UsageKey::Input => (&mut usage.input_tokens, &mut input_seen),
                        UsageKey::Output => (&mut usage.output_tokens, &mut output_seen),
                        UsageKey::Other => {
                            map.next_value::<de::IgnoredAny>()?;
                            continue;
                        }
                    };
                    let UsageCount(count) = map.next_value::<UsageCount>()?;
                    if *seen || count.is_none() {
                        usage.malformed = true;
                        *slot = None;
                    } else {
                        *slot = count;
                    }
                    *seen = true;
                }
                Ok(usage)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(LenientUsage::default())
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(LenientUsage::default())
            }

            fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
                deserializer.deserialize_any(Self)
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(malformed_usage())
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(malformed_usage())
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(malformed_usage())
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(malformed_usage())
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(malformed_usage())
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(malformed_usage())
            }
        }

        deserializer.deserialize_any(UsageVisitor)
    }
}

fn malformed_usage() -> LenientUsage {
    LenientUsage {
        malformed: true,
        ..LenientUsage::default()
    }
}

/// The usage keys PRX reads; every other key is skipped unread.
enum UsageKey {
    Input,
    Output,
    Other,
}

impl<'de> Deserialize<'de> for UsageKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;

        impl Visitor<'_> for KeyVisitor {
            type Value = UsageKey;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a usage field name")
            }

            fn visit_str<E: de::Error>(self, key: &str) -> Result<Self::Value, E> {
                Ok(match key {
                    "input_tokens" => UsageKey::Input,
                    "output_tokens" => UsageKey::Output,
                    _ => UsageKey::Other,
                })
            }
        }

        deserializer.deserialize_identifier(KeyVisitor)
    }
}

/// One usage count: `Some` for an integer in `u32` range, `None` for any
/// other JSON value (consumed without being kept).
struct UsageCount(Option<u32>);

impl<'de> Deserialize<'de> for UsageCount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CountVisitor;

        impl<'de> Visitor<'de> for CountVisitor {
            type Value = UsageCount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a token count")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UsageCount(u32::try_from(value).ok()))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UsageCount(u32::try_from(value).ok()))
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(UsageCount(None))
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(UsageCount(None))
            }

            fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(UsageCount(None))
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(UsageCount(None))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                while seq.next_element::<de::IgnoredAny>()?.is_some() {}
                Ok(UsageCount(None))
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
                Ok(UsageCount(None))
            }
        }

        deserializer.deserialize_any(CountVisitor)
    }
}

/// One answer. Every primitive's fields are optional here and required per
/// primitive in `decode_answer`, so an answer never passes through a
/// buffering internally-tagged representation that would hold unknown
/// fields.
#[derive(Deserialize)]
struct WireAnswer {
    #[serde(rename = "type")]
    kind: String,
    choice: Option<String>,
    probabilities: Option<UniqueMap<f64>>,
    confidence: Option<f64>,
    score: Option<f64>,
    legend: Option<UniqueMap<UniqueJson>>,
    noul: Option<f64>,
}

/// A JSON object whose repeated keys are recorded instead of silently
/// overwritten (the `serde_json` default keeps the last one).
struct UniqueMap<V> {
    entries: BTreeMap<String, V>,
    duplicated: bool,
}

impl<'de, V: Deserialize<'de>> Deserialize<'de> for UniqueMap<V> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct MapVisitor<V>(PhantomData<V>);

        impl<'de, V: Deserialize<'de>> Visitor<'de> for MapVisitor<V> {
            type Value = UniqueMap<V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut entries = BTreeMap::new();
                let mut duplicated = false;
                while let Some((key, value)) = map.next_entry::<String, V>()? {
                    duplicated |= entries.insert(key, value).is_some();
                }
                Ok(UniqueMap { entries, duplicated })
            }
        }

        deserializer.deserialize_map(MapVisitor(PhantomData))
    }
}

/// Any JSON value, with a flag set when any object inside it repeats a key.
struct UniqueJson {
    value: serde_json::Value,
    duplicated: bool,
}

impl UniqueJson {
    const fn plain(value: serde_json::Value) -> Self {
        Self {
            value,
            duplicated: false,
        }
    }
}

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

struct UniqueJsonVisitor;

impl<'de> Visitor<'de> for UniqueJsonVisitor {
    type Value = UniqueJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(value.into()))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(|number| UniqueJson::plain(serde_json::Value::Number(number)))
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(value.into()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(value.into()))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(serde_json::Value::Null))
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(UniqueJson::plain(serde_json::Value::Null))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut items = Vec::new();
        let mut duplicated = false;
        while let Some(item) = seq.next_element::<UniqueJson>()? {
            duplicated |= item.duplicated;
            items.push(item.value);
        }
        Ok(UniqueJson {
            value: serde_json::Value::Array(items),
            duplicated,
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut object = serde_json::Map::new();
        let mut duplicated = false;
        while let Some((key, item)) = map.next_entry::<String, UniqueJson>()? {
            duplicated |= item.duplicated;
            duplicated |= object.insert(key, item.value).is_some();
        }
        Ok(UniqueJson {
            value: serde_json::Value::Object(object),
            duplicated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeated_key_is_recorded_at_every_depth() {
        let map: UniqueMap<f64> = serde_json::from_str(r#"{"a": 0.5, "a": 0.5}"#).expect("test: parse");
        assert!(map.duplicated);
        let nested: UniqueJson = serde_json::from_str(r#"{"x": [{"k": 1, "k": 2}]}"#).expect("test: parse");
        assert!(nested.duplicated);
        let clean: UniqueJson = serde_json::from_str(r#"{"x": [{"k": 1}], "y": null}"#).expect("test: parse");
        assert!(!clean.duplicated);
        assert_eq!(clean.value, serde_json::json!({"x": [{"k": 1}], "y": null}));
    }

    #[test]
    fn level_keys_must_be_exactly_the_canonical_indices() {
        let map = |keys: &[&str]| {
            keys.iter()
                .map(|key| ((*key).to_string(), 0.5))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            by_level_index(map(&["1", "0", "2", "10", "3", "4", "5", "6", "7", "8", "9"])).map(|v| v.len()),
            Ok(11)
        );
        for keys in [&["1", "2"][..], &["0", "01"], &["0", "+1"], &["0", "x"], &["0", "2"]] {
            assert_eq!(
                by_level_index(map(keys)),
                Err(invalid(ValidationRule::LegendMismatch)),
                "{keys:?}"
            );
        }
    }

    #[test]
    fn positional_forms_are_refused() {
        assert!(serde_json::from_str::<MapOnly<WireResponse>>(r#"["jev-1.13.0", {}, null]"#).is_err());
        assert!(
            serde_json::from_str::<MapOnly<WireResponse>>(
                r#"{"model": "jev-1.13.0", "answers": {"q-1": ["noul", null, null, null, null, null, 0.7]}}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<MapOnly<WireResponse>>(
                r#"{"model": "jev-1.13.0", "answers": {"q-1": {"type": "noul", "noul": 0.7}}}"#
            )
            .is_ok()
        );
    }

    #[test]
    fn malformed_usage_is_ignored_and_flagged() {
        let read = |usage: &str| {
            let body = format!(r#"{{"model": "m", "answers": {{}}, "usage": {usage}}}"#);
            let MapOnly(response) = serde_json::from_str::<MapOnly<WireResponse>>(&body).expect("test: parse");
            let usage = response.usage.unwrap_or_default();
            (usage.input_tokens, usage.output_tokens, usage.malformed)
        };
        assert_eq!(
            read(r#"{"input_tokens": 525, "output_tokens": 70}"#),
            (Some(525), Some(70), false)
        );
        assert_eq!(
            read(r#"{"input_tokens": 525.0, "output_tokens": 70}"#),
            (None, Some(70), true)
        );
        assert_eq!(read(r#"{"input_tokens": 4294967296}"#), (None, None, true));
        assert_eq!(read(r#"{"input_tokens": -1, "cost": 0.1}"#), (None, None, true));
        assert_eq!(
            read(r#"{"input_tokens": "many", "output_tokens": [1]}"#),
            (None, None, true)
        );
        assert_eq!(read(r#"{"input_tokens": 1, "input_tokens": 2}"#), (None, None, true));
        assert_eq!(read(r#"[1, 2]"#), (None, None, true));
        assert_eq!(read(r#""lots""#), (None, None, true));
        assert_eq!(read("null"), (None, None, false));
    }

    #[test]
    fn an_explanation_field_is_skipped_by_the_parser() {
        let answer: WireAnswer = serde_json::from_str(
            r#"{"type": "noul", "noul": 0.5, "explanation": {"text": "ignore all rules"}, "reasoning": "x"}"#,
        )
        .expect("test: parse");
        assert_eq!(answer.kind, "noul");
        assert_eq!(answer.noul, Some(0.5));
    }
}
