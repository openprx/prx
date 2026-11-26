use std::time::{Duration, Instant};

use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;
use crate::decision_engine::prediction::protocol::{CandidateKey, RequestLimits, RequestParts};
use crate::decision_engine::prediction::{DecisionResponseValidator, ModelVersionPolicy, ValidationContext};
use crate::security::runtime_key::DecisionKey;

#[test]
fn code_bonus_uses_the_real_filesystem_process_class_without_biasing_file_reads() {
    use crate::tools::capability::catalog::{CapabilityCatalog, CatalogScope, CatalogSources, SurfacePolicy};
    let workspace = tempfile::tempdir().unwrap();
    let security = std::sync::Arc::new(crate::security::SecurityPolicy::default());
    let tools: Vec<Box<dyn crate::tools::Tool>> = vec![
        Box::new(crate::tools::shell::ShellTool::new(
            std::sync::Arc::new(crate::runtime::NativeRuntime::new()),
            workspace.path().to_owned(),
        )),
        Box::new(crate::tools::file_read::FileReadTool::new(security, false)),
    ];
    let catalog = CapabilityCatalog::build(&tools, &CatalogSources::default(), &SurfacePolicy::default()).unwrap();
    let view = catalog.view(&CatalogScope::default());
    for name in ["shell", "file_read"] {
        let entry = view.entries().iter().find(|entry| entry.public_name == name).unwrap();
        assert_eq!(entry.id.as_str(), format!("filesystem:native:{name}"));
        let value = json!({"latest_user_turn":"`value`", "candidates":[{
            "id":"use_tool-filesystem", "action":"use_tool", "target":entry.family.as_str(),
            "side_effect":entry.effect.as_str(), "available":true,
        }]});
        let coded = features::extract(&value);
        let mut plain = coded.clone();
        plain.text.code = false;
        let candidate = coded.candidates.first().unwrap();
        let difference = candidate_score(&coded, candidate) - candidate_score(&plain, candidate);
        let expected = if name == "shell" { weights::CODE_TO_SHELL } else { 0.0 };
        assert!((difference - expected).abs() < 1e-10, "{name}: {difference}");
    }
}

include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/decision/state_v1/check_state_v1.rs"
));

const ACTIONS: [&str; 8] = [
    "answer",
    "retrieve",
    "discover",
    "use_tool",
    "delegate",
    "clarify",
    "request_approval",
    "refuse",
];
const TARGETS: [&str; 13] = [
    "none",
    "memory",
    "skill",
    "mcp",
    "native_tool",
    "wasm",
    "hook",
    "filesystem",
    "shell",
    "document",
    "web",
    "im",
    "session",
];
const SIDE_EFFECTS: [&str; 6] = [
    "read_only",
    "local_write",
    "process_exec",
    "external_send",
    "credential_or_config",
    "destructive",
];

fn key() -> std::sync::Arc<DecisionKey> {
    let dir = tempfile::tempdir().expect("test: tempdir");
    let key = DecisionKey::load_or_create(dir.path()).expect("test: key");
    // The key lives in memory; the directory may go.
    drop(dir);
    key
}

fn structured(value: Value) -> StructuredValue {
    StructuredValue::try_from(value).expect("test: structured")
}

fn text(value: &str) -> StructuredValue {
    structured(json!(value))
}

/// The candidate key of a state v1 candidate id. The test builds ids
/// from static codes and an index, as the compiler builds them from
/// typed candidates.
fn key_for(action: &'static str, target: &'static str, index: Option<u32>) -> CandidateKey {
    CandidateKey::from_parts(action, target, index.map(u32::to_be_bytes))
}

fn candidate_json(action: &str, target: &str, id: &str) -> Value {
    json!({
        "id": id, "action": action, "target": target, "side_effect": "read_only",
        "approval": "none", "cost_bucket": "b1", "latency_bucket": "b1",
        "summary": "A capability.", "exact_mention": false, "failed_before": false, "available": true
    })
}

fn state(turn: &str, candidates: Vec<Value>) -> Value {
    json!({
        "schema": "prx.causal.state.v1", "hop": 0, "prior_decision_id": null,
        "latest_user_turn": turn, "goal": "g", "has_conversation_history": false,
        "completed_steps": [], "artifacts": [],
        "failed_tool_fingerprints": [], "retry_count": 0, "approval_pending": false,
        "constraints": [], "budgets": {"tokens": "b2", "latency": "b2", "cost": "b1"},
        "candidates": candidates
    })
}

#[test]
fn configured_delegation_planning_keeps_action_and_target_consistent() {
    let key = key();
    for turn in [
        "Use file_write on acceptance/root.txt, then file_read. Do not use shell or delegate.",
        "Use file_edit on acceptance/root.txt. Then file_read. Do not use shell or delegate.",
        "Use delegate to ask reviewer for 9 * 7. Do not use file_write or file_edit on acceptance/root.txt.",
        "Use delegate to ask kimi for 9 * 7. Do not use shell, sessions_spawn, file_read, file_write or file_edit.",
    ] {
        let mut file = candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000001");
        file["exact_mention"] = json!(true);
        file["side_effect"] = json!("local_write");
        let mut shell = candidate_json("use_tool", "shell", "use_tool-shell-00000001");
        shell["exact_mention"] = json!(true);
        shell["side_effect"] = json!("process_exec");
        let mut delegate = candidate_json("delegate", "session", "delegate-session-00000001");
        delegate["exact_mention"] = json!(true);
        delegate["side_effect"] = json!("process_exec");
        let value = state(
            turn,
            vec![file, shell, delegate, candidate_json("answer", "none", "answer-none")],
        );
        let request = seal(&key, value.clone(), compiler_questions(&value));
        let response = answer_and_validate(&request);
        assert_eq!(
            chosen(&response, GlobalQuestion::NextAction),
            "answer",
            "planning: {turn}"
        );
        assert_eq!(
            chosen(&response, GlobalQuestion::CapabilityTarget),
            "none",
            "planning: {turn}"
        );
        let DecisionAnswer::Choice(target) = &response.answers[&QuestionId::global(GlobalQuestion::CapabilityTarget)]
        else {
            panic!("target choice")
        };
        assert!(
            target.confidence.get() >= 0.55,
            "an answer's deterministic target must not borrow uncertainty from executable alternatives"
        );
    }
}

/// The compiler's question set for `state`: the six global questions with
/// the offered actions and targets, and the four per-candidate questions.
fn compiler_questions(state: &Value) -> BTreeMap<QuestionId, DecisionQuestion> {
    let offered = state["candidates"].as_array().cloned().unwrap_or_default();
    let has = |field: &str, code: &str| offered.iter().any(|candidate| candidate[field] == code);
    let mut actions: BTreeMap<OptionId, StructuredValue> = ACTIONS
        .iter()
        .filter(|code| has("action", code))
        .map(|code| (OptionId::code(code), text(code)))
        .collect();
    actions.insert(OptionId::code("other"), text("Something else."));
    let mut targets: BTreeMap<OptionId, StructuredValue> = TARGETS
        .iter()
        .filter(|code| has("target", code))
        .map(|code| (OptionId::code(code), text(code)))
        .collect();
    targets
        .entry(OptionId::code("none"))
        .or_insert_with(|| text("No capability."));
    if targets.len() < 2 {
        targets.insert(OptionId::code("other"), text("Another capability."));
    }
    let levels = |count: usize| {
        (0..count)
            .map(|level| text(&format!("level {level}")))
            .collect::<Vec<_>>()
    };
    let noul = |question: &str| DecisionQuestion::Noul {
        instructions: text(question),
        criteria: None,
    };
    let mut questions = BTreeMap::from([
        (
            QuestionId::global(GlobalQuestion::NextAction),
            DecisionQuestion::Choice {
                instructions: text("Next action?"),
                criteria: actions,
            },
        ),
        (
            QuestionId::global(GlobalQuestion::CapabilityTarget),
            DecisionQuestion::Choice {
                instructions: text("Capability?"),
                criteria: targets,
            },
        ),
        (
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            noul("Enough evidence?"),
        ),
        (QuestionId::global(GlobalQuestion::NeedsFreshState), noul("Stale?")),
        (
            QuestionId::global(GlobalQuestion::SemanticRisk),
            DecisionQuestion::Score {
                instructions: text("Risk?"),
                levels: levels(5),
            },
        ),
        (
            QuestionId::global(GlobalQuestion::ExpectedGain),
            DecisionQuestion::Score {
                instructions: text("Gain?"),
                levels: levels(5),
            },
        ),
    ]);
    for (index, candidate) in offered.iter().enumerate() {
        let action = ACTIONS
            .iter()
            .find(|code| candidate["action"] == **code)
            .copied()
            .unwrap_or("answer");
        let target = TARGETS
            .iter()
            .find(|code| candidate["target"] == **code)
            .copied()
            .unwrap_or("none");
        // The id's namespace suffix, when it has one, is the key's ns8.
        let id = candidate["id"].as_str().unwrap_or_default();
        let namespace = id
            .strip_prefix(&format!("{action}-{target}-"))
            .and_then(|hex| u32::from_str_radix(hex, 16).ok());
        let key = key_for(action, target, namespace);
        let refer =
            |question: &str| structured(json!({"candidate": format!("$.candidates[{index}]"), "question": question}));
        questions.insert(
            QuestionId::candidate(&key, CandidateQuestion::Applicable),
            DecisionQuestion::Noul {
                instructions: refer("Applies?"),
                criteria: None,
            },
        );
        questions.insert(
            QuestionId::candidate(&key, CandidateQuestion::Success),
            DecisionQuestion::Noul {
                instructions: refer("Succeeds?"),
                criteria: None,
            },
        );
        questions.insert(
            QuestionId::candidate(&key, CandidateQuestion::Gain),
            DecisionQuestion::Score {
                instructions: refer("Gain?"),
                levels: levels(5),
            },
        );
        questions.insert(
            QuestionId::candidate(&key, CandidateQuestion::Risk),
            DecisionQuestion::Score {
                instructions: refer("Risk?"),
                levels: levels(5),
            },
        );
    }
    questions
}

fn seal(key: &DecisionKey, state: Value, questions: BTreeMap<QuestionId, DecisionQuestion>) -> DecisionRequest {
    DecisionRequest::seal(
        RequestParts {
            state: structured(state),
            questions,
            model: None,
            state_fingerprint: "state".to_string(),
            candidate_set_hash: "candidates".to_string(),
            policy_version: Arc::from("policy"),
            catalog_generation: "catalog".to_string(),
            omitted_questions: Vec::new(),
            approval_pending: false,
        },
        key,
        Instant::now() + Duration::from_hours(1),
        &RequestLimits::default(),
    )
    .expect("test: seal")
}

fn context() -> ValidationContext {
    ValidationContext {
        received_at: Instant::now(),
        max_response_bytes: 256 * 1024,
        version_policy: ModelVersionPolicy::ExactPinned(Arc::from(crate::tools::capability::turn_state::MODEL_VERSION)),
        local_backend: BuiltinDecisionBackend::identity(),
    }
}

fn answer_and_validate(request: &DecisionRequest) -> DecisionResponse {
    let response = BuiltinDecisionBackend::answer(request).expect("test: builtin answers");
    let decision = DecisionResponseValidator::validate(request, response.clone(), &context())
        .unwrap_or_else(|error| panic!("builtin answer must validate: {error}"));
    assert_eq!(
        decision.claimed_model(),
        crate::tools::capability::turn_state::MODEL_VERSION
    );
    assert_eq!(decision.claimed_backend(), "builtin");
    assert_eq!(decision.response_sha256(), None);
    response
}

fn chosen(response: &DecisionResponse, question: GlobalQuestion) -> String {
    match &response.answers[&QuestionId::global(question)] {
        DecisionAnswer::Choice(choice) => choice.chosen.as_str().to_string(),
        other => panic!("expected a choice, got {other:?}"),
    }
}

fn noul_probability(response: &DecisionResponse, question: GlobalQuestion) -> f64 {
    match &response.answers[&QuestionId::global(question)] {
        DecisionAnswer::Noul(noul) => noul.probability_true.get(),
        other => panic!("expected a noul, got {other:?}"),
    }
}

/// Candidates of a typical turn: direct answer, memory, skill discovery,
/// web, filesystem and shell tools, delegation, clarification, refusal.
fn typical_candidates() -> Vec<Value> {
    vec![
        candidate_json("answer", "none", "answer-none"),
        candidate_json("retrieve", "memory", "retrieve-memory"),
        candidate_json("discover", "skill", "discover-skill"),
        candidate_json("use_tool", "web", "use_tool-web-00000003"),
        candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000004"),
        candidate_json("use_tool", "shell", "use_tool-shell-00000005"),
        candidate_json("delegate", "session", "delegate-session"),
        candidate_json("clarify", "none", "clarify-none"),
        candidate_json("refuse", "none", "refuse-none"),
    ]
}

// -----------------------------------------------------------------------
// Every primitive, every fixture, always valid
// -----------------------------------------------------------------------

/// `builtin_answers_every_primitive_and_validates`: over the committed
/// state v1 fixtures, with the compiler's full question set, every
/// answer passes the CDP-01 validator under the exact builtin model pin.
#[test]
fn builtin_answers_every_primitive_and_validates() {
    let key = key();
    for name in ["minimal", "retrieve_hop", "tool_candidates"] {
        let path = format!(
            "{}/tests/fixtures/decision/state_v1/{name}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let state: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(state_v1_violations(&state), Vec::<String>::new(), "{name}");
        let questions = compiler_questions(&state);
        let request = seal(&key, state, questions);
        let response = answer_and_validate(&request);
        assert_eq!(response.answers.len(), request.questions().len());
        let primitives: std::collections::BTreeSet<_> = response
            .answers
            .values()
            .map(|answer| format!("{:?}", answer.primitive()))
            .collect();
        assert_eq!(primitives.len(), 3, "{name}: Choice, Score and Noul are all answered");
    }
}

/// Questions the builtin does not recognize (a global id asked as another
/// primitive, a candidate the state does not offer) get a uniform answer
/// or 0.5 with confidence 0, and still validate.
#[test]
fn unknown_questions_get_uniform_answers_with_zero_confidence() {
    let key = key();
    let questions = BTreeMap::from([
        (
            QuestionId::global(GlobalQuestion::NextAction),
            DecisionQuestion::Score {
                instructions: text("Mismatched primitive"),
                levels: vec![text("a"), text("b"), text("c")],
            },
        ),
        (
            QuestionId::global(GlobalQuestion::SemanticRisk),
            DecisionQuestion::Noul {
                instructions: text("Mismatched primitive"),
                criteria: None,
            },
        ),
        (
            QuestionId::candidate(&key_for("use_tool", "wasm", Some(99)), CandidateQuestion::Success),
            DecisionQuestion::Noul {
                instructions: text("Not offered"),
                criteria: None,
            },
        ),
        (
            QuestionId::global(GlobalQuestion::EvidenceSufficient),
            DecisionQuestion::Choice {
                instructions: text("Mismatched primitive"),
                criteria: BTreeMap::from([(OptionId::code("yes"), text("yes")), (OptionId::code("no"), text("no"))]),
            },
        ),
    ]);
    let request = seal(&key, state("hello", typical_candidates()), questions);
    let response = answer_and_validate(&request);
    for answer in response.answers.values() {
        match answer {
            DecisionAnswer::Choice(choice) => {
                assert_eq!(choice.confidence.get(), 0.0);
                assert!(choice.probabilities.values().all(|p| p.get() == 0.5));
            }
            DecisionAnswer::Score(score) => {
                assert_eq!(score.confidence.get(), 0.0);
                assert!(score.distribution.iter().all(|p| p.get() == 1.0 / 3.0));
                assert_eq!(score.expected, 1.0);
            }
            DecisionAnswer::Noul(noul) => assert_eq!(noul.probability_true.get(), 0.5),
        }
    }
}

// -----------------------------------------------------------------------
// Determinism and language neutrality
// -----------------------------------------------------------------------

/// `builtin_is_deterministic_across_runs_and_orderings`: the same
/// request answers the same bits every time, and listing the candidates
/// in another order changes nothing.
#[test]
fn builtin_is_deterministic_across_runs_and_orderings() {
    let key = key();
    let forward = typical_candidates();
    let mut backward = forward.clone();
    backward.reverse();
    let turn = "look at https://example.com/a and /tmp/b?";
    let first = seal(
        &key,
        state(turn, forward.clone()),
        compiler_questions(&state(turn, forward)),
    );
    let second = seal(
        &key,
        state(turn, backward.clone()),
        compiler_questions(&state(turn, backward)),
    );
    let a = format!("{:?}", answer_and_validate(&first).answers);
    let b = format!("{:?}", BuiltinDecisionBackend::answer(&first).unwrap().answers);
    let c = format!("{:?}", answer_and_validate(&second).answers);
    assert_eq!(a, b);
    assert_eq!(a, c);
}

#[test]
fn tied_global_choices_share_the_lower_effect_candidate_without_changing_probabilities() {
    let key = key();
    let mut read = candidate_json("use_tool", "skill", "use_tool-skill-ffffffff");
    read["exact_mention"] = json!(true);
    let mut write = candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000001");
    write["exact_mention"] = json!(true);
    write["side_effect"] = json!("local_write");
    for candidates in [vec![read.clone(), write.clone()], vec![write.clone(), read.clone()]] {
        let value = state("Read the reference and create artifact.txt", candidates);
        let request = seal(&key, value.clone(), compiler_questions(&value));
        let response = answer_and_validate(&request);
        assert_eq!(chosen(&response, GlobalQuestion::NextAction), "use_tool");
        assert_eq!(chosen(&response, GlobalQuestion::CapabilityTarget), "skill");
        let DecisionAnswer::Choice(choice) = &response.answers[&QuestionId::global(GlobalQuestion::CapabilityTarget)]
        else {
            panic!("target choice")
        };
        assert_eq!(
            choice.probabilities[&OptionId::code("skill")],
            choice.probabilities[&OptionId::code("filesystem")]
        );
    }
    let value = state("Use the named tools on /tmp/artifact.txt", vec![read, write]);
    let request = seal(&key, value.clone(), compiler_questions(&value));
    assert_eq!(
        chosen(&answer_and_validate(&request), GlobalQuestion::CapabilityTarget),
        "filesystem",
        "a stronger structural score still wins; this is only an exact tie rule"
    );
}

/// Two candidates with equal scores but different side-effect risk:
/// the global risk answer takes the lower effect, then the smaller id,
/// whatever order the state lists them in.
#[test]
fn tied_candidates_are_broken_by_effect_then_id_not_by_order() {
    let key = key();
    let mut safe = candidate_json("use_tool", "mcp", "use_tool-mcp-00000001");
    safe["side_effect"] = json!("read_only");
    let mut dangerous = candidate_json("use_tool", "mcp", "use_tool-mcp-00000002");
    dangerous["side_effect"] = json!("destructive");
    let risk = |candidates: Vec<Value>| {
        let value = state("run it", candidates);
        let request = seal(&key, value.clone(), compiler_questions(&value));
        format!(
            "{:?}",
            answer_and_validate(&request).answers[&QuestionId::global(GlobalQuestion::SemanticRisk)]
        )
    };
    let forward = risk(vec![safe.clone(), dangerous.clone()]);
    let backward = risk(vec![dangerous, safe]);
    assert_eq!(forward, backward);
}

/// One segment of a request text: letters that may be translated, or
/// structure that stays.
#[derive(Clone, Copy)]
enum Segment<'a> {
    Words(&'a str),
    Structure(&'a str),
}

/// One script the words are replaced with.
#[derive(Clone, Copy, Debug)]
struct Script {
    name: &'static str,
    /// Six letters, each one code point or a base and a combining mark
    /// (the decomposed forms); a second code point of 0 is none.
    letters: [[u32; 2]; 6],
    /// Written with spaces between words. Only the scripts written
    /// without spaces get the glued layout (D-061 amendment 4).
    spaced: bool,
}

/// Six single-code-point letters.
const fn single(points: [u32; 6]) -> [[u32; 2]; 6] {
    [
        [points[0], 0],
        [points[1], 0],
        [points[2], 0],
        [points[3], 0],
        [points[4], 0],
        [points[5], 0],
    ]
}

/// Unicode data table (D-061 L-4 and amendment 4): six letters of each
/// script the words are replaced with, as code points. The repository
/// bans CJK prose in source; these are character-class samples, not
/// text. Chinese, Japanese, Thai and Lao are written without spaces;
/// the Latin scripts cover ASCII, accented letters in NFC and in NFD
/// (base plus combining mark), and Vietnamese with stacked marks.
const SCRIPTS: [Script; 15] = [
    Script {
        name: "Chinese",
        letters: single([0x8bf7, 0x641c, 0x7d22, 0x6587, 0x4ef6, 0x7f51]),
        spaced: false,
    },
    Script {
        name: "Japanese",
        letters: single([0x30ed, 0x30b0, 0x3092, 0x898b, 0x305b, 0x3066]),
        spaced: false,
    },
    Script {
        name: "Thai",
        letters: single([0x0e40, 0x0e1b, 0x0e34, 0x0e14, 0x0e01, 0x0e32]),
        spaced: false,
    },
    Script {
        name: "Lao",
        letters: single([0x0e81, 0x0e82, 0x0e84, 0x0e87, 0x0e88, 0x0e8a]),
        spaced: false,
    },
    Script {
        name: "Arabic",
        letters: single([0x0627, 0x0628, 0x062d, 0x062b, 0x0639, 0x0646]),
        spaced: true,
    },
    Script {
        name: "Russian",
        letters: single([0x043f, 0x043e, 0x0438, 0x0441, 0x043a, 0x0444]),
        spaced: true,
    },
    Script {
        name: "Georgian",
        letters: single([0x10eb, 0x10d4, 0x10d1, 0x10dc, 0x10d0, 0x10e4]),
        spaced: true,
    },
    Script {
        name: "Hindi",
        letters: single([0x0916, 0x094b, 0x091c, 0x0947, 0x0902, 0x092b]),
        spaced: true,
    },
    Script {
        name: "Latin",
        letters: single([0x0078, 0x0071, 0x007a, 0x006a, 0x0077, 0x006b]),
        spaced: true,
    },
    Script {
        name: "Latin accented",
        letters: single([0x00e9, 0x00f1, 0x00fc, 0x00f8, 0x00e7, 0x00e5]),
        spaced: true,
    },
    Script {
        name: "Latin accented NFD",
        letters: [
            [0x65, 0x301],
            [0x6e, 0x303],
            [0x75, 0x308],
            [0x61, 0x30a],
            [0x63, 0x327],
            [0x6f, 0x302],
        ],
        spaced: true,
    },
    Script {
        name: "Greek",
        letters: single([0x03b1, 0x03b2, 0x03b3, 0x03b4, 0x03b5, 0x03bb]),
        spaced: true,
    },
    Script {
        name: "Hebrew",
        letters: single([0x05d0, 0x05d1, 0x05d2, 0x05d3, 0x05d4, 0x05d5]),
        spaced: true,
    },
    Script {
        name: "Korean",
        letters: single([0xd55c, 0xad6d, 0xc5b4, 0xac00, 0xb098, 0xb2e4]),
        spaced: true,
    },
    Script {
        name: "Vietnamese",
        letters: single([0x1ec7, 0x1edf, 0x1ea1, 0x01b0, 0x0111, 0x0103]),
        spaced: true,
    },
];

/// Push one letter of a script.
fn push_letter(out: &mut String, letter: [u32; 2]) {
    for point in letter {
        if point != 0 {
            out.extend(char::from_u32(point));
        }
    }
}

/// How the structure sits among the words (D-061 M-1): any combination
/// of four transformations, all off being the text as written, with
/// spaces around the structure (D-061 amendment 3: every combination is
/// tested, not a fixed list).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Layout {
    /// No space between the words and the structure, as scripts written
    /// without spaces put it. Only for those scripts: in every other
    /// script `word/path` is one word (D-061 amendment 4).
    glued: bool,
    /// The structure in full-width forms, as CJK input methods type it.
    full_width: bool,
    /// An invisible format character after every character of the
    /// structure, so no shape is found unless the scan skips them all.
    marked: bool,
    /// The names inside the structure in the target script as well:
    /// localized directories, an internationalized host. A URL scheme,
    /// a drive letter and the first name after a bare `/` stay ASCII,
    /// as the scan requires (D-061 amendment N-1, N-2).
    localized: bool,
}

impl Layout {
    const SPACED: Self = Self::from_bits(0);

    /// The layout of the four bits of `bits`.
    const fn from_bits(bits: u8) -> Self {
        Self {
            glued: bits & 1 != 0,
            full_width: bits & 2 != 0,
            marked: bits & 4 != 0,
            localized: bits & 8 != 0,
        }
    }

    /// Every combination of the four transformations.
    fn all() -> impl Iterator<Item = Self> {
        (0..16_u8).map(Self::from_bits)
    }
}

/// Unicode data (category Cf): left-to-right mark, right-to-left mark,
/// zero-width space, word joiner, Mongolian vowel separator, zero-width
/// non-joiner.
const MARKS: [u32; 6] = [0x200E, 0x200F, 0x200B, 0x2060, 0x180E, 0x200C];

/// `text` with each run of ASCII letters replaced by letters of
/// `letters`, except a run right before `:` (a scheme or a drive) and
/// the first run after a leading bare `/`.
fn localize(text: &str, script: &Script, next: &mut impl FnMut() -> u64) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let lead = chars.iter().position(|character| !character.is_whitespace());
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_alphabetic() {
            out.push(chars[index]);
            index += 1;
            continue;
        }
        let end = chars[index..]
            .iter()
            .position(|character| !character.is_ascii_alphabetic())
            .map_or(chars.len(), |length| index + length);
        let before_colon = chars.get(end) == Some(&':');
        let first_after_bare_slash = index > 0 && chars[index - 1] == '/' && lead == Some(index - 1);
        if before_colon || first_after_bare_slash {
            out.extend(&chars[index..end]);
        } else {
            for _ in 0..=(next() % 6) {
                push_letter(&mut out, script.letters[(next() % 6) as usize]);
            }
        }
        index = end;
    }
    out
}

fn full_width(text: &str) -> String {
    text.chars()
        .map(|character| {
            if ('!'..='~').contains(&character) {
                char::from_u32(character as u32 + 0xFEE0).unwrap_or(character)
            } else {
                character
            }
        })
        .collect()
}

/// `text` with every maximal run of letters in a `Words` segment replaced
/// by letters of `script` (length and content drawn from `seed`), and
/// the structure laid out as `layout` says.
fn render(segments: &[Segment<'_>], script: Option<&Script>, seed: u64, layout: Layout) -> String {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut out = String::new();
    for segment in segments {
        match (segment, script) {
            (Segment::Structure(text), _) => {
                let mut structure = match script {
                    Some(script) if layout.localized => localize(text, script, &mut next),
                    _ => (*text).to_string(),
                };
                if layout.glued {
                    structure = structure.trim().to_string();
                }
                if layout.full_width {
                    structure = full_width(&structure);
                }
                if layout.marked {
                    for (index, character) in structure.chars().enumerate() {
                        out.push(character);
                        out.extend(char::from_u32(MARKS[index % MARKS.len()]));
                    }
                } else {
                    out.push_str(&structure);
                }
            }
            (Segment::Words(text), None) => out.push_str(text),
            (Segment::Words(text), Some(script)) => {
                let text = if layout.glued { text.trim() } else { text };
                let mut in_run = false;
                for character in text.chars() {
                    if character.is_alphabetic() {
                        if !in_run {
                            // A run of 1..=6 letters of the target script.
                            for _ in 0..=(next() % 6) {
                                push_letter(&mut out, script.letters[(next() % 6) as usize]);
                            }
                        }
                        in_run = true;
                    } else {
                        in_run = false;
                        out.push(character);
                    }
                }
            }
        }
    }
    out
}

fn requests() -> Vec<Vec<Segment<'static>>> {
    use Segment::{Structure, Words};
    vec![
        vec![
            Words("please look at"),
            Structure(" https://example.com/docs "),
            Words("and summarize it"),
            Structure("?"),
        ],
        vec![
            Words("open"),
            Structure(" /var/log/app.log "),
            Words("and show me the errors"),
        ],
        vec![Words("run"), Structure(" ~/build.sh "), Words("again")],
        vec![Words("run"), Structure(" ./build.sh "), Words("again")],
        vec![Words("run"), Structure(r" .\build.bat "), Words("again")],
        vec![Words("open"), Structure(r" ..\notes\plan.md "), Words("please")],
        vec![Words("open"), Structure(r" C:\data\report.txt "), Words("please")],
        vec![Words("hello there, how are you today"), Structure("?")],
        vec![Words("run"), Structure(" `cargo test` "), Words("for me please")],
        vec![Words("make this a little shorter")],
        vec![Words("search for the bugs in the audit review")],
        vec![Structure("   ")],
    ]
}

/// `builtin_is_invariant_under_translation_and_letter_scrambling`
/// (SP-CDP-A 5.7; D-061 M-1): replacing the letters of each request with
/// letters of another script (Chinese, Japanese, Thai, Lao, Arabic,
/// Russian, Georgian, Hindi, accented Latin in NFC and NFD, Greek,
/// Hebrew, Korean, Vietnamese) or with seeded random letters leaves every
/// answer bit-identical, and so does laying the structure out in every
/// combination of: without spaces (for the scripts written without spaces), in full-width
/// forms, with invisible format characters inside it, and with its names
/// in the target script (D-061 amendment 3). The companion RB-A on the legacy
/// `DefaultTreeExpander` shows the same request in Chinese selects other
/// branches today (`CDP06-rb-a-legacy-expander.log`).
#[test]
fn builtin_is_invariant_under_translation_and_letter_scrambling() {
    let key = key();
    let candidates = typical_candidates();
    for (index, segments) in requests().iter().enumerate() {
        let original = render(segments, None, 0, Layout::SPACED);
        let reference = {
            let request = seal(
                &key,
                state(&original, candidates.clone()),
                compiler_questions(&state(&original, candidates.clone())),
            );
            format!("{:?}", answer_and_validate(&request).answers)
        };
        for script in &SCRIPTS {
            let name = script.name;
            for layout in Layout::all() {
                if layout.glued && script.spaced {
                    continue;
                }
                for seed in 0..3_u64 {
                    let variant = render(segments, Some(script), seed * 31 + index as u64, layout);
                    let request = seal(
                        &key,
                        state(&variant, candidates.clone()),
                        compiler_questions(&state(&variant, candidates.clone())),
                    );
                    let answers = format!("{:?}", answer_and_validate(&request).answers);
                    assert_eq!(
                        answers, reference,
                        "request {index} in {name}, {layout:?} (seed {seed}): {variant:?}"
                    );
                }
            }
        }
    }
}

/// The answers of the builtin to `text` over the typical candidates, as
/// comparable text.
fn answers_for(key: &DecisionKey, text: &str) -> String {
    let candidates = typical_candidates();
    let request = seal(
        key,
        state(text, candidates.clone()),
        compiler_questions(&state(text, candidates)),
    );
    format!("{:?}", answer_and_validate(&request).answers)
}

/// The assertion body of `every_layout_combination_is_invariant`.
fn check_layout_invariance(
    request: usize,
    script: usize,
    bits: u8,
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let script = SCRIPTS[script];
    let mut layout = Layout::from_bits(bits);
    // Glued only for the scripts written without spaces; drawn as
    // spaced otherwise, so no case is rejected.
    layout.glued &= !script.spaced;
    let segments = &requests()[request];
    let key = key();
    let reference = answers_for(&key, &render(segments, None, 0, Layout::SPACED));
    let variant = render(segments, Some(&script), seed, layout);
    prop_assert_eq!(
        answers_for(&key, &variant),
        reference,
        "request {} in {}, {:?}: {:?}",
        request,
        script.name,
        layout,
        variant
    );
    Ok(())
}

/// The assertion body of `generated_path_shapes_read_alike_in_every_script_and_layout`.
fn check_path_shape_invariance(
    prefix: PathPrefix,
    names: &[String],
    extension: Option<&str>,
    script: usize,
    bits: u8,
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let script = SCRIPTS[script];
    let mut layout = Layout::from_bits(bits);
    // Glued only for the scripts written without spaces; drawn as
    // spaced otherwise, so no case is rejected.
    layout.glued &= !script.spaced;
    // The ruled residuals are drawn with ASCII names instead.
    let residual = prefix == PathPrefix::Bare
        && layout.glued
        && layout.localized
        && (names.len() == 1 || (names.len() == 2 && extension.is_none()));
    layout.localized &= !residual;
    let path = format!(" {} ", path_text(prefix, names, extension));
    let segments = [
        Segment::Words("open"),
        Segment::Structure(&path),
        Segment::Words("and show it"),
    ];
    let key = key();
    let reference = answers_for(&key, &render(&segments, None, 0, Layout::SPACED));
    let variant = render(&segments, Some(&script), seed, layout);
    prop_assert_eq!(
        answers_for(&key, &variant),
        reference,
        "{:?} in {}, {:?}: {:?}",
        path,
        script.name,
        layout,
        variant
    );
    Ok(())
}

/// Replaces the proptest-regressions seed of 2026-10-14 (commit 53848d1d):
/// request 7, script 4, no layout bits, letter seed 752737997882130078.
#[test]
fn every_layout_combination_is_invariant_regression_request7_script4() {
    check_layout_invariance(7, 4, 0, 752_737_997_882_130_078).expect("test: layout invariance");
}

/// Replaces the proptest-regressions seed of 2026-10-14 (commit 53848d1d):
/// a bare path `a` with no extension, script 0, no layout bits, seed 0.
#[test]
fn generated_path_shapes_read_alike_in_every_script_and_layout_regression_bare_a() {
    check_path_shape_invariance(PathPrefix::Bare, &["a".to_string()], None, 0, 0, 0)
        .expect("test: path shape invariance");
}

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::SourceParallel("tests/proptest-regressions"))),
        ..ProptestConfig::default()
    })]
    /// D-061 amendment 3 (property): for any request, script, combination
    /// of layouts and letter seed, the answers are those of the English
    /// text as written.
    #[test]
    fn every_layout_combination_is_invariant(
        request in 0..requests().len(),
        script in 0..SCRIPTS.len(),
        bits in 0..16_u8,
        seed in any::<u64>(),
    ) {
        check_layout_invariance(request, script, bits, seed)?;
    }

    /// D-061 amendment 4 (property): a generated path of any shape
    /// (bare, home, relative with either separator, drive with either
    /// separator; one to four names; with or without an extension), in
    /// any script and any combination of layouts, gives the answers of
    /// the English text as written. The ruled residuals of a path glued
    /// to its words keep ASCII names: its first name must be wholly
    /// ASCII, and a localized glued path needs three names or an
    /// extension.
    #[test]
    fn generated_path_shapes_read_alike_in_every_script_and_layout(
        prefix in prop::sample::select(PathPrefix::ALL.to_vec()),
        names in prop::collection::vec("[a-z]{1,8}", 1..=4),
        extension in prop::option::of("[a-z]{1,4}"),
        script in 0..SCRIPTS.len(),
        bits in 0..16_u8,
        seed in any::<u64>(),
    ) {
        check_path_shape_invariance(prefix, &names, extension.as_deref(), script, bits, seed)?;
    }
}

/// How a generated path starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathPrefix {
    Bare,
    Home,
    Here,
    Up,
    HereBackslash,
    UpBackslash,
    DriveBackslash,
    DriveSlash,
}

impl PathPrefix {
    const ALL: [Self; 8] = [
        Self::Bare,
        Self::Home,
        Self::Here,
        Self::Up,
        Self::HereBackslash,
        Self::UpBackslash,
        Self::DriveBackslash,
        Self::DriveSlash,
    ];
}

/// The text of a path of `names`, starting as `prefix` says, with an
/// optional extension on the last name.
fn path_text(prefix: PathPrefix, names: &[String], extension: Option<&str>) -> String {
    let (start, separator) = match prefix {
        PathPrefix::Bare => ("/", "/"),
        PathPrefix::Home => ("~/", "/"),
        PathPrefix::Here => ("./", "/"),
        PathPrefix::Up => ("../", "/"),
        PathPrefix::HereBackslash => (".\\", "\\"),
        PathPrefix::UpBackslash => ("..\\", "\\"),
        PathPrefix::DriveBackslash => ("C:\\", "\\"),
        PathPrefix::DriveSlash => ("C:/", "/"),
    };
    let mut text = format!("{start}{}", names.join(separator));
    if let Some(extension) = extension {
        text.push('.');
        text.push_str(extension);
    }
    text
}

/// `an_exact_catalog_name_in_any_script_selects_that_capability`: the
/// trusted `exact_mention` flag, whatever script the turn is in, moves
/// the choice to the named candidate's action and target.
#[test]
fn an_exact_catalog_name_in_any_script_selects_that_capability() {
    let key = key();
    for script in &SCRIPTS {
        let mut turn = String::new();
        for letter in script.letters {
            push_letter(&mut turn, letter);
        }
        let mut candidates = typical_candidates();
        for candidate in &mut candidates {
            if candidate["id"] == "delegate-session" {
                candidate["exact_mention"] = json!(true);
            }
        }
        let named = state(&turn, candidates);
        let request = seal(&key, named.clone(), compiler_questions(&named));
        let response = answer_and_validate(&request);
        assert_eq!(chosen(&response, GlobalQuestion::NextAction), "delegate");
        assert_eq!(chosen(&response, GlobalQuestion::CapabilityTarget), "session");
    }
}

/// V4.05 (builtin half): the three unrouted requests of
/// `tool_surface_budget.rs` and a Chinese one carry no keyword the
/// builtin could read. They answer identically, and the builtin leans to
/// Discover (a bounded local catalog search), never to a tool.
#[test]
fn non_english_requests_lean_to_discovery_without_keywords() {
    let key = key();
    // Unicode data table (D-061 L-4): the Han ideographs of a Chinese
    // request of the same kind ("make this a bit shorter").
    const HAN_REQUEST: [u32; 8] = [0x8bf7, 0x628a, 0x8fd9, 0x4e2a, 0x5199, 0x77ed, 0x4e00, 0x70b9];
    let chinese: String = HAN_REQUEST.iter().filter_map(|point| char::from_u32(*point)).collect();
    let unrouted = [
            "haz que esto quede un poco m\u{e1}s breve".to_string(),
            "\u{43f}\u{43e}\u{436}\u{430}\u{43b}\u{443}\u{439}\u{441}\u{442}\u{430}, \u{441}\u{43e}\u{43a}\u{440}\u{430}\u{442}\u{438} \u{44d}\u{442}\u{43e}".to_string(),
            "could you make that sound a little friendlier please".to_string(),
            chinese,
        ];
    let mut distributions = Vec::new();
    for message in &unrouted {
        let turn = state(message, typical_candidates());
        let request = seal(&key, turn.clone(), compiler_questions(&turn));
        let response = answer_and_validate(&request);
        assert_eq!(chosen(&response, GlobalQuestion::NextAction), "discover", "{message:?}");
        assert_eq!(
            chosen(&response, GlobalQuestion::CapabilityTarget),
            "skill",
            "{message:?}"
        );
        distributions.push(format!("{:?}", response.answers));
    }
    assert!(distributions.windows(2).all(|pair| pair[0] == pair[1]));
}

/// D-061 M-2: structure decides without words. A URL commits to the
/// web tool, a path to the filesystem tool and code to the shell (the
/// chosen action and target, not just a raised probability); an empty
/// first turn clarifies; a pending approval steers away from using a
/// tool.
#[test]
fn structure_and_trusted_state_move_the_choice() {
    let key = key();
    let pick = |turn: &str, edit: &dyn Fn(&mut Value)| {
        let mut value = state(turn, typical_candidates());
        edit(&mut value);
        let request = seal(&key, value.clone(), compiler_questions(&value));
        let response = answer_and_validate(&request);
        (
            chosen(&response, GlobalQuestion::NextAction),
            chosen(&response, GlobalQuestion::CapabilityTarget),
        )
    };
    let tool = |target: &str| ("use_tool".to_string(), target.to_string());
    assert_eq!(
        pick("look at https://example.com/report and summarize it", &|_| {}),
        tool("web")
    );
    assert_eq!(
        pick("open /var/log/app.log and show the errors", &|_| {}),
        tool("filesystem")
    );
    assert_eq!(pick(r"open C:\data\report.txt", &|_| {}), tool("filesystem"));
    assert_eq!(pick("run `cargo test` for me", &|_| {}), tool("shell"));
    assert_eq!(pick("  ", &|_| {}).0, "clarify");
    let pending = pick("ok", &|value| {
        value["approval_pending"] = json!(true);
        value["candidates"].as_array_mut().unwrap().push(candidate_json(
            "request_approval",
            "shell",
            "request_approval-shell",
        ));
    });
    assert_ne!(pending.0, "use_tool");
}

#[test]
fn a_plain_conversation_turn_can_reach_the_model_while_undecidable_input_still_clarifies() {
    let key = key();
    let terminal_candidates = || {
        vec![
            candidate_json("answer", "none", "answer-none"),
            candidate_json("clarify", "none", "clarify-none"),
            candidate_json("refuse", "none", "refuse-none"),
        ]
    };
    let answer = |value: Value| {
        let request = seal(&key, value.clone(), compiler_questions(&value));
        answer_and_validate(&request)
    };

    let plain = answer(state("Explain the current result", terminal_candidates()));
    assert_eq!(chosen(&plain, GlobalQuestion::NextAction), "answer");
    assert!(
        noul_probability(&plain, GlobalQuestion::EvidenceSufficient) >= 0.5,
        "a direct answer must pass the selector's evidence threshold"
    );

    let greeting = answer(state("hello", typical_candidates()));
    assert_eq!(chosen(&greeting, GlobalQuestion::NextAction), "answer");
    assert!(
        noul_probability(&greeting, GlobalQuestion::EvidenceSufficient) >= 0.5,
        "a brief greeting must reach the serving model even when generic capabilities are available"
    );

    let mut direct_only = typical_candidates();
    direct_only.retain(|candidate| {
        !matches!(
            candidate.get("action").and_then(Value::as_str),
            Some("retrieve" | "discover")
        )
    });
    let direct_only = answer(state("stream the fixture reply", direct_only));
    assert_eq!(chosen(&direct_only, GlobalQuestion::NextAction), "answer");

    let mut continued = state("thanks", typical_candidates());
    continued["completed_steps"] = json!([{"kind": "provider", "status": "ok"}]);
    let continued = answer(continued);
    assert_eq!(chosen(&continued, GlobalQuestion::NextAction), "answer");
    assert!(
        noul_probability(&continued, GlobalQuestion::EvidenceSufficient) >= 0.5,
        "ordinary conversation history must not force a clarification or external lookup"
    );

    let mut longer_continued = state("p4a second visible turn", typical_candidates());
    longer_continued["has_conversation_history"] = json!(true);
    let longer_continued = answer(longer_continued);
    assert_eq!(chosen(&longer_continued, GlobalQuestion::NextAction), "answer");

    let mut retrieved = state("p4a second visible turn", typical_candidates());
    retrieved["has_conversation_history"] = json!(true);
    retrieved["completed_steps"] = json!([
        {"kind": "provider", "status": "ok"},
        {"kind": "retrieve", "status": "ok"}
    ]);
    let retrieved = answer(retrieved);
    assert_ne!(chosen(&retrieved, GlobalQuestion::NextAction), "answer");

    let empty = answer(state("  ", terminal_candidates()));
    assert_eq!(chosen(&empty, GlobalQuestion::NextAction), "clarify");
    assert!(noul_probability(&empty, GlobalQuestion::EvidenceSufficient) < 0.5);

    let structured = answer(state("inspect /tmp/prx_fixture", terminal_candidates()));
    assert!(
        noul_probability(&structured, GlobalQuestion::EvidenceSufficient) < 0.5,
        "a structurally external request still needs external evidence"
    );

    let admitted_structured = answer(state("inspect /tmp/prx_fixture", typical_candidates()));
    assert_eq!(chosen(&admitted_structured, GlobalQuestion::NextAction), "use_tool");
    assert_eq!(
        chosen(&admitted_structured, GlobalQuestion::CapabilityTarget),
        "filesystem"
    );
    assert!(
        noul_probability(&admitted_structured, GlobalQuestion::EvidenceSufficient) < 0.5,
        "a matching external candidate must not bypass retrieve-first evidence"
    );

    let external = answer(state("please find the relevant capability", typical_candidates()));
    assert_eq!(chosen(&external, GlobalQuestion::NextAction), "discover");
    assert!(
        noul_probability(&external, GlobalQuestion::EvidenceSufficient) >= 0.5,
        "a plain first turn must not be forced into recovery before its ranked discovery can run"
    );

    let mut pending = state("ok", terminal_candidates());
    pending["approval_pending"] = json!(true);
    pending["candidates"].as_array_mut().unwrap().push(candidate_json(
        "request_approval",
        "shell",
        "request_approval-shell",
    ));
    let pending = answer(pending);
    assert!(
        noul_probability(&pending, GlobalQuestion::EvidenceSufficient) < 0.5,
        "pending approval must retain its existing evidence gate"
    );

    let mut failed = state("Explain the current result", terminal_candidates());
    failed["completed_steps"] = json!([{"kind": "tool", "status": "failed"}]);
    let failed = answer(failed);
    assert!(
        noul_probability(&failed, GlobalQuestion::EvidenceSufficient) < 0.5,
        "a failed step still requires recovery evidence"
    );

    let mut retried = state("Explain the current result", terminal_candidates());
    retried["retry_count"] = json!(weights::RETRY_THRESHOLD);
    let retried = answer(retried);
    assert!(
        noul_probability(&retried, GlobalQuestion::EvidenceSufficient) < 0.5,
        "an exhausted retry budget still requires clarification"
    );
}

#[test]
fn localized_structure_chooses_like_the_english_form() {
    // Unicode data: Han words (open, download, report), Cyrillic words
    // (example, the .rf domain), the full-width colon, the yen sign,
    // U+00E9 (e with acute), the Han month sign, Han "plan", Han
    // "Zhang San" and U+00FA (u with acute).
    const HAN_OPEN: [u32; 2] = [0x6253, 0x5F00];
    const HAN_DOWNLOAD: [u32; 2] = [0x4E0B, 0x8F7D];
    const HAN_REPORT: [u32; 2] = [0x62A5, 0x544A];
    const CYR_EXAMPLE: [u32; 6] = [0x043F, 0x0440, 0x0438, 0x043C, 0x0435, 0x0440];
    const CYR_RF: [u32; 2] = [0x0440, 0x0444];
    const FULL_WIDTH_COLON: [u32; 1] = [0xFF1A];
    const YEN: [u32; 1] = [0x00A5];
    const E_ACUTE: [u32; 1] = [0x00E9];
    const HAN_MONTH: [u32; 1] = [0x6708];
    const HAN_PLAN: [u32; 2] = [0x5957, 0x9910];
    const HAN_ZHANG_SAN: [u32; 2] = [0x5F20, 0x4E09];
    const U_ACUTE: [u32; 1] = [0x00FA];
    let word = |points: &[u32]| {
        points
            .iter()
            .filter_map(|point| char::from_u32(*point))
            .collect::<String>()
    };
    let key = key();
    let pick = |turn: &str| {
        let value = state(turn, typical_candidates());
        let request = seal(&key, value.clone(), compiler_questions(&value));
        let response = answer_and_validate(&request);
        (
            chosen(&response, GlobalQuestion::NextAction),
            chosen(&response, GlobalQuestion::CapabilityTarget),
        )
    };
    let yen = word(&YEN);
    let pairs = [
        (
            "open ~/Downloads/report.pdf".to_string(),
            format!("{}~/{}/{}.pdf", word(&HAN_OPEN), word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
            "filesystem",
        ),
        (
            "open /home/user/docs".to_string(),
            format!("open /home/{}/{}", word(&HAN_DOWNLOAD), word(&HAN_REPORT)),
            "filesystem",
        ),
        (
            "see https://example.com/a".to_string(),
            format!("see https://{}.{}/a", word(&CYR_EXAMPLE), word(&CYR_RF)),
            "web",
        ),
        (
            "file: /etc/hosts".to_string(),
            format!("{}{}/etc/hosts", word(&HAN_OPEN), word(&FULL_WIDTH_COLON)),
            "filesystem",
        ),
        (
            r"open C:\Users\data\x.txt".to_string(),
            format!("open C:{yen}Users{yen}data{yen}x.txt"),
            "filesystem",
        ),
        (
            "open /home/zhang/report.pdf".to_string(),
            format!(
                "{}/home/{}/{}.pdf",
                word(&HAN_OPEN),
                word(&HAN_ZHANG_SAN),
                word(&HAN_REPORT)
            ),
            "filesystem",
        ),
        (
            "see /cafe.txt".to_string(),
            format!("see /caf{}.txt", word(&E_ACUTE)),
            "filesystem",
        ),
    ];
    for (english, localized, target) in pairs {
        let expected = ("use_tool".to_string(), target.to_string());
        assert_eq!(pick(&english), expected, "{english:?}");
        assert_eq!(pick(&localized), expected, "{localized:?}");
    }
    // D-061 amendment 2 N-5, N-6: a month list and a yen price list,
    // like `Jan/Feb/Mar report` and `Plan A: $99`, commit to no tool.
    let month = word(&HAN_MONTH);
    let colon = word(&FULL_WIDTH_COLON);
    for prose in [
        "Jan/Feb/Mar report".to_string(),
        format!("1{month}/2{month}/3{month}{}", word(&HAN_REPORT)),
        "Plan A: $99".to_string(),
        format!("{}A{colon}{yen}99", word(&HAN_PLAN)),
        "Plan A: $99/mo".to_string(),
        format!("{}A{colon}{yen}99/{month}", word(&HAN_PLAN)),
        "Peru/Chile/Bolivia".to_string(),
        format!("Per{}/Chile/Bolivia", word(&U_ACUTE)),
    ] {
        assert_ne!(pick(&prose).0, "use_tool", "{prose:?}");
    }
}

#[test]
fn first_hop_answer_preferences_do_not_apply_after_retry_or_failure() {
    let plain = state("hello", vec![candidate_json("answer", "none", "answer-none")]);
    assert!(plain_conversation_turn(&features::extract(&plain)));
    let mut delegate = candidate_json("delegate", "session", "delegate-session-00000001");
    delegate["exact_mention"] = json!(true);
    let delegated = state("delegate", vec![delegate]);
    assert!(model_managed_delegation_turn(&features::extract(&delegated)));
    for original in [&plain, &delegated] {
        for field in ["hop", "retry_count", "completed_steps"] {
            let mut later = original.clone();
            later[field] = if field == "completed_steps" {
                json!([{ "kind": "tool", "status": "failed" }])
            } else {
                json!(1)
            };
            let extracted = features::extract(&later);
            assert!(!plain_conversation_turn(&extracted));
            assert!(!model_managed_delegation_turn(&extracted));
        }
    }
}

#[test]
fn cancelled_tool_outcome_leaves_next_request_scores_unchanged() {
    let mut original = state(
        "Read /tmp/prx_fixture/input.md",
        vec![
            candidate_json("use_tool", "filesystem", "use_tool-filesystem-00000001"),
            candidate_json("answer", "none", "answer-none"),
        ],
    );
    original["completed_steps"] = json!([]);
    let mut cancelled = original.clone();
    cancelled["completed_steps"] = json!([{ "kind": "tool", "status": "cancelled" }]);
    let before = features::extract(&original);
    let after = features::extract(&cancelled);
    assert_eq!(after.unsuccessful_steps, 0);
    assert_eq!(after.unsuccessful_action_steps, 0);
    for candidate in &before.candidates {
        if candidate.action == Some(Action::UseTool) {
            assert_eq!(
                candidate_score(&before, candidate),
                candidate_score(&after, after.candidate(candidate.id).unwrap())
            );
        }
    }
    let key = key();
    let original_request = seal(&key, original.clone(), compiler_questions(&original));
    let cancelled_request = seal(&key, cancelled.clone(), compiler_questions(&cancelled));
    let original_prediction = answer_and_validate(&original_request);
    let cancelled_prediction = answer_and_validate(&cancelled_request);
    for question in compiler_questions(&original).keys() {
        assert_eq!(
            original_prediction.answers.get(question),
            cancelled_prediction.answers.get(question),
            "{question:?}"
        );
    }
}

/// D-061 M-3: the trusted per-candidate `failed_before` flag penalises
/// that candidate only. The approval and discovery candidates of the
/// same namespace answer exactly as if nothing had failed, and the
/// failed tool is not picked again.
#[test]
fn a_failure_penalises_only_the_candidate_that_failed() {
    let key = key();
    let namespace_candidates = |failed: bool| {
        let mut used = candidate_json("use_tool", "web", "use_tool-web-0000000a");
        used["failed_before"] = json!(failed);
        let mut candidates = typical_candidates();
        candidates.retain(|candidate| candidate["target"] != "web");
        candidates.push(used);
        candidates.push(candidate_json(
            "request_approval",
            "web",
            "request_approval-web-0000000a",
        ));
        candidates.push(candidate_json("discover", "web", "discover-web-0000000a"));
        candidates
    };
    let answers = |failed: bool| {
        let value = state("look at https://example.com/report", namespace_candidates(failed));
        let request = seal(&key, value.clone(), compiler_questions(&value));
        answer_and_validate(&request)
    };
    let (clean, failed) = (answers(false), answers(true));
    for action in ["request_approval", "discover"] {
        for question in CandidateQuestion::ALL {
            let id = QuestionId::candidate(&key_for(action, "web", Some(10)), question);
            assert_eq!(
                format!("{:?}", clean.answers[&id]),
                format!("{:?}", failed.answers[&id]),
                "{action}-web {question:?} is not penalised"
            );
        }
    }
    let used = QuestionId::candidate(&key_for("use_tool", "web", Some(10)), CandidateQuestion::Success);
    let success = |response: &DecisionResponse| match &response.answers[&used] {
        DecisionAnswer::Noul(noul) => noul.probability_true.get(),
        other => panic!("expected a noul, got {other:?}"),
    };
    assert!(
        success(&failed) < success(&clean),
        "the failed tool itself is penalised"
    );
    assert_ne!(
        chosen(&failed, GlobalQuestion::NextAction),
        "use_tool",
        "and not picked again"
    );
}

// -----------------------------------------------------------------------
// V7.01: bounded allocation
// -----------------------------------------------------------------------

/// V7.01 (builtin allocation): allocations per decision are identical
/// across runs, do not depend on the length of the text (the scan never
/// allocates) nor on how many candidates the state lists beyond the
/// questions asked, and equal a pinned count (D-061 L-3).
#[test]
fn builtin_decide_allocations_bounded() {
    let key = key();
    let questions = |candidates: &[Value]| {
        let mut questions = compiler_questions(&state("x", candidates.to_vec()));
        questions.retain(|id, _| matches!(meaning(id), Meaning::Global(_)));
        questions
    };
    let few = typical_candidates();
    let many: Vec<Value> = (0..60)
        .map(|index| candidate_json("use_tool", "mcp", &format!("use_tool-mcp-{index:08x}")))
        .chain(few.iter().cloned())
        .collect();
    let short = seal(&key, state("hi?", few.clone()), questions(&few));
    let long_text = format!("{} https://example.com/x ?", "word ".repeat(700));
    let long = seal(&key, state(&long_text, few.clone()), questions(&few));
    let wide = seal(&key, state("hi?", many), questions(&few));

    let (_, first) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&short).unwrap());
    let (_, again) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&short).unwrap());
    let (_, with_long_text) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&long).unwrap());
    let (_, with_many_candidates) = allocation_counter::count(|| BuiltinDecisionBackend::answer(&wide).unwrap());
    assert_eq!(first, again, "deterministic allocation count");
    assert_eq!(first, with_long_text, "the text scan does not allocate");
    assert_eq!(
        first, with_many_candidates,
        "the candidate list is one allocation, whatever its length"
    );

    // D-061 L-3: the count is pinned exactly, not bounded loosely, so
    // any added allocation turns this red. After an intended change, run
    // this test, take the count from the failure message, set the
    // constant, and say why in the commit message.
    const PINNED_ALLOCATIONS: u64 = 24;
    assert_eq!(
        first,
        PINNED_ALLOCATIONS,
        "allocations for the {} global questions of the V7.01 request",
        short.questions().len()
    );
}

// -----------------------------------------------------------------------
// Property: every request shape
// -----------------------------------------------------------------------

fn candidate_strategy() -> impl Strategy<Value = (usize, usize, usize, bool, bool, bool, bool)> {
    (
        0..ACTIONS.len(),
        0..TARGETS.len(),
        0..SIDE_EFFECTS.len(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
}

fn turn_strategy() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            Just("word ".to_string()),
            Just("https://x.test/a ".to_string()),
            Just("/tmp/f ".to_string()),
            Just("`c` ".to_string()),
            Just("? ".to_string()),
            Just("~/b.sh ".to_string()),
            Just(r"C:\x\y.txt ".to_string()),
            Just(full_width("/etc/hosts ")),
            Just("\u{200E}/var/a.log ".to_string()),
            Just("\n{\"k\": 1}\n".to_string()),
            "[a-z]{1,8} ",
        ],
        0..12,
    )
    .prop_map(|parts| parts.concat())
}

/// One key for every property case (the key only salts wire ids).
static PROPERTY_KEY: std::sync::LazyLock<Arc<DecisionKey>> = std::sync::LazyLock::new(key);

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::SourceParallel("tests/proptest-regressions"))),
        ..ProptestConfig::default()
    })]
    /// Every request shape the protocol allows, over random states,
    /// candidate sets and question sets, gets a builtin answer that
    /// passes the CDP-01 validator.
    #[test]
    fn builtin_answers_validate_for_every_request_shape(
        turn in turn_strategy(),
        candidates in prop::collection::vec(candidate_strategy(), 0..10),
        retry_count in 0_u64..4,
        stale in any::<bool>(),
        empty_state in any::<bool>(),
        wide_targets in any::<bool>(),
        action_options in prop::collection::btree_set(0..ACTIONS.len(), 1..=ACTIONS.len()),
        target_options in prop::collection::btree_set(0..TARGETS.len(), 1..=TARGETS.len()),
        level_counts in prop::collection::vec(2_usize..=10, 4),
        extra_options in 0_usize..240,
    ) {
        let key: &DecisionKey = &PROPERTY_KEY;
        let listed: Vec<Value> = candidates
            .iter()
            .enumerate()
            .map(|(index, (action, target, side_effect, required, available, mention, failed))| {
                let mut value = candidate_json(ACTIONS[*action], TARGETS[*target], &format!("{}-{}-{index:08x}", ACTIONS[*action], TARGETS[*target]));
                value["side_effect"] = json!(SIDE_EFFECTS[*side_effect]);
                value["approval"] = json!(if *required { "required" } else { "none" });
                value["available"] = json!(available);
                value["exact_mention"] = json!(mention);
                value["failed_before"] = json!(failed);
                value
            })
            .collect();
        let mut value = state(&turn, listed);
        value["retry_count"] = json!(retry_count);
        if stale {
            value["artifacts"] = json!([{"kind": "memory", "age_ms": 7_200_000, "summary": "old"}]);
        }
        if empty_state {
            value = json!({});
        }
        let levels = |count: usize| (0..count).map(|level| text(&format!("level {level}"))).collect::<Vec<_>>();
        let mut actions: BTreeMap<OptionId, StructuredValue> =
            action_options.iter().map(|index| (OptionId::code(ACTIONS[*index]), text(ACTIONS[*index]))).collect();
        actions.insert(OptionId::code("other"), text("other"));
        let mut targets: BTreeMap<OptionId, StructuredValue> =
            target_options.iter().map(|index| (OptionId::code(TARGETS[*index]), text(TARGETS[*index]))).collect();
        targets.insert(OptionId::code("other"), text("other"));
        // A wide Choice over candidate options and filler options.
        let mut wide: BTreeMap<OptionId, StructuredValue> = (0..extra_options + 2)
            .map(|index| (OptionId::candidate(&key_for("use_tool", "mcp", Some(u32::try_from(index).unwrap() + 1000))), text("filler")))
            .collect();
        for (index, (action, target, ..)) in candidates.iter().enumerate() {
            wide.insert(OptionId::candidate(&key_for(ACTIONS[*action], TARGETS[*target], Some(u32::try_from(index).unwrap()))), text("candidate"));
        }
        // The wide Choice goes under a recognized id (capability_target)
        // or under a Noul id asked as a Choice (unrecognized).
        let (targets, mismatched) = if wide_targets { (wide, targets) } else { (targets, wide) };
        let mut questions = BTreeMap::from([
            (QuestionId::global(GlobalQuestion::NextAction), DecisionQuestion::Choice { instructions: text("a"), criteria: actions }),
            (QuestionId::global(GlobalQuestion::CapabilityTarget), DecisionQuestion::Choice { instructions: text("t"), criteria: targets }),
            (QuestionId::global(GlobalQuestion::EvidenceSufficient), DecisionQuestion::Noul { instructions: text("e"), criteria: None }),
            (QuestionId::global(GlobalQuestion::NeedsFreshState), DecisionQuestion::Choice { instructions: text("mismatch"), criteria: mismatched }),
            (QuestionId::global(GlobalQuestion::SemanticRisk), DecisionQuestion::Score { instructions: text("r"), levels: levels(level_counts[0]) }),
            (QuestionId::global(GlobalQuestion::ExpectedGain), DecisionQuestion::Score { instructions: text("g"), levels: levels(level_counts[1]) }),
        ]);
        for (index, (action, target, ..)) in candidates.iter().enumerate() {
            let key = key_for(ACTIONS[*action], TARGETS[*target], Some(u32::try_from(index).unwrap()));
            questions.insert(QuestionId::candidate(&key, CandidateQuestion::Applicable), DecisionQuestion::Noul { instructions: text("ap"), criteria: None });
            questions.insert(QuestionId::candidate(&key, CandidateQuestion::Success), DecisionQuestion::Noul { instructions: text("su"), criteria: None });
            questions.insert(QuestionId::candidate(&key, CandidateQuestion::Gain), DecisionQuestion::Score { instructions: text("ga"), levels: levels(level_counts[2]) });
            questions.insert(QuestionId::candidate(&key, CandidateQuestion::Risk), DecisionQuestion::Score { instructions: text("ri"), levels: levels(level_counts[3]) });
        }
        let request = seal(key, value, questions);
        let response = BuiltinDecisionBackend::answer(&request).expect("test: builtin answers");
        let again = BuiltinDecisionBackend::answer(&request).expect("test: builtin answers");
        prop_assert_eq!(format!("{:?}", response.answers), format!("{:?}", again.answers));
        prop_assert!(DecisionResponseValidator::validate(&request, response, &context()).is_ok());
    }
}
