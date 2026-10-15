use std::env;

use anyhow::{Context, Result, ensure};
use browser_engine::{DocumentRuntime, FormField};

const DEFAULT_SEED: u64 = 0x5eed_cafe_2026_1012;
const DEFAULT_CASES: usize = 64;
const VALUES: [&str; 8] = [
    "",
    "plain",
    "Ada λ",
    "漢字",
    "🙂",
    "line one\nline two",
    "e\u{301}",
    "zero \u{0} tail",
];
const TYPED: [&str; 6] = ["A", "λ!", "漢字", "🙂!", "xy", "!Z"];

#[derive(Clone, Copy)]
struct Generator(u64);

impl Generator {
    fn next(&mut self) -> u64 {
        let mut value = self.0;
        value ^= value << 13;
        value ^= value >> 7;
        value ^= value << 17;
        self.0 = value;
        value
    }

    fn index(&mut self, length: usize) -> usize {
        (self.next() as usize) % length
    }

    fn flag(&mut self) -> bool {
        self.next() & 1 == 1
    }
}

#[derive(Debug)]
struct Config {
    seed: u64,
    cases: usize,
}

fn parse_number(value: &str) -> Result<u64> {
    if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
            .with_context(|| format!("invalid hexadecimal number {value:?}"))
    } else {
        value
            .parse::<u64>()
            .with_context(|| format!("invalid decimal number {value:?}"))
    }
}

fn parse_config() -> Result<Config> {
    let mut config = Config {
        seed: DEFAULT_SEED,
        cases: DEFAULT_CASES,
    };
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--seed" => {
                let value = arguments.next().context("--seed requires a value")?;
                config.seed = parse_number(&value)?;
            }
            "--cases" => {
                let value = arguments.next().context("--cases requires a value")?;
                config.cases = usize::try_from(parse_number(&value)?)
                    .context("case count does not fit this platform")?;
                ensure!(config.cases > 0, "case count must be positive");
                ensure!(config.cases <= 10_000, "case count must not exceed 10000");
            }
            other => anyhow::bail!("unknown argument {other:?}; expected --seed or --cases"),
        }
    }
    Ok(config)
}

fn expected_typed(initial: &str, typed: &str, cancel_bang: bool) -> String {
    let mut expected = initial.to_owned();
    for character in typed.chars() {
        if !(cancel_bang && character == '!') {
            expected.push(character);
        }
    }
    expected
}

fn run_case(index: usize, generator: &mut Generator) -> Result<()> {
    let fill_value = VALUES[generator.index(VALUES.len())];
    let typed = TYPED[generator.index(TYPED.len())];
    let cancel_bang = generator.flag();
    let cancel_click = generator.flag();
    let click_first = generator.flag();
    let initial_checked = generator.flag();
    let checked_attribute = if initial_checked { " checked" } else { "" };
    let role = if generator.flag() { "editor" } else { "reader" };
    let include_rust = generator.flag();
    let include_web = generator.flag();
    let include_lambda = generator.flag();
    let include_terms = generator.flag();
    let publish = generator.flag();
    let cancel_submit = generator.flag();
    let html = format!(
        "<!doctype html><html><body><div id='parent'><button id='action'>act</button><input id='check' type='checkbox'{checked_attribute}><input id='edit' type='text' value=''><input id='disabled' type='text' value='locked' disabled></div><form id='form' action='/cases' method='post'><input id='required' name='query' required><select id='role' name='role'><option value='reader' selected>Reader</option><option value='editor'>Editor</option></select><select id='tags' name='tag' multiple><option value='rust'>Rust</option><option value='web'>Web</option><option value='λ'>Lambda</option></select><input id='terms' name='terms' type='checkbox' value='yes'><input name='ignored' value='no' disabled><button id='save' name='intent' value='save' type='submit'>Save</button><button id='publish' name='intent' value='publish' type='submit'>Publish</button></form><p id='events'>idle</p><p id='completion'>pending</p><p id='form-event'>idle</p></body></html>"
    );
    let mut runtime = DocumentRuntime::from_html(&html)
        .with_context(|| format!("case {index}: create runtime"))?;
    let action = runtime.element_target_by_id("action")?;
    let check = runtime.element_target_by_id("check")?;
    let edit = runtime.element_target_by_id("edit")?;
    let disabled = runtime.element_target_by_id("disabled")?;
    let events = runtime.element_target_by_id("events")?;
    let completion = runtime.element_target_by_id("completion")?;
    let form = runtime.element_target_by_id("form")?;
    let required = runtime.element_target_by_id("required")?;
    let role_select = runtime.element_target_by_id("role")?;
    let tags_select = runtime.element_target_by_id("tags")?;
    let terms = runtime.element_target_by_id("terms")?;
    let save = runtime.element_target_by_id("save")?;
    let publish_button = runtime.element_target_by_id("publish")?;
    let form_event = runtime.element_target_by_id("form-event")?;
    let script = format!(
        r#"
        const action = document.getElementById("action");
        action.addEventListener("click", (event) => {{
            document.getElementById("events").textContent = "clicked";
            if ({cancel_click}) event.preventDefault();
        }});
        document.getElementById("edit").addEventListener("keydown", (event) => {{
            if ({cancel_bang} && event.key === "!") event.preventDefault();
        }});
        __rwbAwaitHostCompletion("active").then((value) => {{
            document.getElementById("completion").textContent = value;
        }});
        document.getElementById("form").addEventListener("submit", (event) => {{
            document.getElementById("form-event").textContent = {cancel_submit} ? "cancelled" : "submitted";
            if ({cancel_submit}) event.preventDefault();
        }});
        "#
    );
    runtime.evaluate_script(&script)?;
    let generation = runtime.document_generation();
    ensure!(
        runtime.complete_host_request(generation, "active", fill_value)?,
        "case {index}: registered host completion was not accepted"
    );
    ensure!(
        runtime.observe_text(&completion)? == fill_value,
        "case {index}: host completion promise mutation was not observable"
    );
    ensure!(
        !runtime.complete_host_request(generation, "active", "duplicate")?,
        "case {index}: completed host request was accepted twice"
    );

    let perform_click = |runtime: &mut DocumentRuntime| -> Result<()> {
        let enabled = runtime.click(&action)?;
        ensure!(
            enabled != cancel_click,
            "case {index}: click cancellation mismatch; cancel={cancel_click}, enabled={enabled}"
        );
        ensure!(
            runtime.observe_text(&events)? == "clicked",
            "case {index}: click listener mutation was not observable"
        );
        Ok(())
    };
    let perform_edit = |runtime: &mut DocumentRuntime| -> Result<()> {
        let frame_before_edit = runtime.render_document_at(800, 600)?;
        runtime.fill(&edit, fill_value)?;
        runtime.type_text(&edit, typed)?;
        let actual = runtime.observe_value(&edit)?;
        let expected = expected_typed(fill_value, typed, cancel_bang);
        ensure!(
            actual == expected,
            "case {index}: edited value mismatch; fill={fill_value:?}, typed={typed:?}, cancel_bang={cancel_bang}, expected={expected:?}, actual={actual:?}"
        );
        let frame_after_edit = runtime.render_document_at(800, 600)?;
        ensure!(
            frame_before_edit.data() != frame_after_edit.data(),
            "case {index}: edited live value did not change rendered frame; expected={expected:?}"
        );
        Ok(())
    };
    if click_first {
        perform_click(&mut runtime)?;
        perform_edit(&mut runtime)?;
    } else {
        perform_edit(&mut runtime)?;
        perform_click(&mut runtime)?;
    }

    ensure!(
        runtime.fill(&disabled, "changed").is_err(),
        "case {index}: disabled input accepted fill"
    );
    ensure!(
        runtime.observe_value(&disabled)? == "locked",
        "case {index}: rejected disabled fill changed live state"
    );
    ensure!(
        runtime.observe_checked(&check)? == initial_checked,
        "case {index}: initial checked state mismatch"
    );
    ensure!(
        runtime.check(&check, !initial_checked)?,
        "case {index}: checkbox transition was not reported"
    );
    ensure!(
        runtime.observe_checked(&check)? != initial_checked,
        "case {index}: checkbox transition did not persist"
    );
    ensure!(
        !runtime.check(&check, !initial_checked)?,
        "case {index}: repeated checkbox state was not idempotent"
    );

    ensure!(
        runtime.submit(&form, Some(&save)).is_err(),
        "case {index}: empty required form control did not block submission"
    );
    ensure!(
        runtime.observe_text(&form_event)? == "idle",
        "case {index}: invalid submission dispatched a submit event"
    );
    let required_value = if fill_value.is_empty() {
        "required"
    } else {
        fill_value
    };
    runtime.fill(&required, required_value)?;
    runtime.select(&role_select, &[role.to_owned()])?;
    let mut requested_tags = Vec::new();
    if include_lambda {
        requested_tags.push("λ".to_owned());
    }
    if include_web {
        requested_tags.push("web".to_owned());
    }
    if include_rust {
        requested_tags.push("rust".to_owned());
    }
    runtime.select(&tags_select, &requested_tags)?;
    let expected_tags = [
        (include_rust, "rust"),
        (include_web, "web"),
        (include_lambda, "λ"),
    ]
    .into_iter()
    .filter_map(|(included, value)| included.then_some(value.to_owned()))
    .collect::<Vec<_>>();
    ensure!(
        runtime.observe_selected_values(&tags_select)? == expected_tags,
        "case {index}: multi-select values were not retained in document order"
    );
    if include_terms {
        runtime.check(&terms, true)?;
    }
    let submitter = if publish { &publish_button } else { &save };
    let actual_submission = runtime.submit(&form, Some(submitter))?;
    if cancel_submit {
        ensure!(
            actual_submission.is_none(),
            "case {index}: cancelled page submission returned a browser submission"
        );
        ensure!(
            runtime.observe_text(&form_event)? == "cancelled",
            "case {index}: cancelled submit listener effect was not observable"
        );
    } else {
        let submission = actual_submission
            .with_context(|| format!("case {index}: valid form submission disappeared"))?;
        let mut expected_fields = vec![
            FormField {
                name: "query".into(),
                value: required_value.into(),
            },
            FormField {
                name: "role".into(),
                value: role.into(),
            },
        ];
        expected_fields.extend(expected_tags.into_iter().map(|value| FormField {
            name: "tag".into(),
            value,
        }));
        if include_terms {
            expected_fields.push(FormField {
                name: "terms".into(),
                value: "yes".into(),
            });
        }
        expected_fields.push(FormField {
            name: "intent".into(),
            value: if publish { "publish" } else { "save" }.into(),
        });
        ensure!(
            submission.method == "post"
                && submission.action == "/cases"
                && submission.fields == expected_fields,
            "case {index}: form serialization mismatch; expected={expected_fields:?}, actual={submission:?}"
        );
        ensure!(
            runtime.observe_text(&form_event)? == "submitted",
            "case {index}: successful submit listener effect was not observable"
        );
    }

    let old_generation = runtime.document_generation();
    runtime.evaluate_script(
        "setTimeout(() => { document.getElementById('events').textContent = 'obsolete'; }, 0); __rwbAwaitHostCompletion('late').then(() => { document.getElementById('events').textContent = 'late-completion'; });",
    )?;
    runtime.navigate("<!doctype html><html><body><p id='events'>replacement</p></body></html>")?;
    ensure!(
        runtime.document_generation() == old_generation + 1,
        "case {index}: navigation did not advance generation"
    );
    ensure!(
        runtime.validate_target(&events).is_err(),
        "case {index}: pre-navigation target remained valid"
    );
    ensure!(
        !runtime.run_next_timer()?,
        "case {index}: obsolete timer survived realm replacement"
    );
    ensure!(
        !runtime.complete_host_request(old_generation, "late", "obsolete")?,
        "case {index}: obsolete host completion entered the replacement realm"
    );
    let replacement = runtime.element_target_by_id("events")?;
    ensure!(
        runtime.observe_text(&replacement)? == "replacement",
        "case {index}: obsolete callback mutated replacement document"
    );
    Ok(())
}

fn main() -> Result<()> {
    let config = parse_config()?;
    let mut generator = Generator(config.seed);
    for index in 0..config.cases {
        run_case(index, &mut generator)?;
    }
    println!(
        "runtime-explore: PASS seed=0x{:016x} cases={} dimensions=unicode,action-order,cancellation,checked,disabled,selection,required-validation,submitter,form-cancellation,host-completion,navigation",
        config.seed, config.cases
    );
    Ok(())
}
