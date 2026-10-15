use std::{
    fs,
    path::PathBuf,
    process,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use browser_engine::{COMPONENT_SCHEMA_VERSION, DocumentRuntime, round_trip_component_record};

fn temporary_database_path() -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!(
        "rwb-component-spike-{}-{nonce}.redb",
        process::id()
    )))
}

fn main() -> Result<()> {
    let mut runtime = DocumentRuntime::from_html(
        "<!doctype html><html><body><main id='status' style='width: 96px; height: 32px'>before</main><p id='timer'>waiting</p><section id='source'>source:<span id='movable'>moved</span></section><section id='destination'>destination:</section><aside id='removable'>remove-me</aside><div id='event-parent'><button id='event-button'>activate</button><input id='event-checkbox' type='checkbox'><input id='check-checkbox' type='checkbox'><input id='radio-a' type='radio' name='choice' checked><input id='radio-b' type='radio' name='choice'><input id='cancel-radio-a' type='radio' name='cancel-choice' checked><input id='cancel-radio-b' type='radio' name='cancel-choice'><input id='text-input' type='text' value='before'><input id='type-input' type='text' value=''><button id='disabled-button' disabled>blocked</button><input id='disabled-input' type='text' value='before' disabled><input id='disabled-checkbox' type='checkbox' disabled></div><p id='event-output'>idle</p><p id='control-output'>idle</p><p id='check-output'>idle</p><p id='fill-output'>idle</p><p id='type-output'>idle</p><p id='disabled-output'>idle</p><form id='fixture-form' action='/submit' method='post'><input id='required-name' name='name' type='text' required value=''><select id='role-select' name='role'><option value='reader' selected>Reader</option><option value='editor'>Editor</option></select><select id='tag-select' name='tag' multiple><option value='rust'>Rust</option><option value='web'>Web</option><option value='λ'>Lambda</option></select><input id='terms-checkbox' name='terms' type='checkbox' value='accepted'><input id='ignored-input' name='ignored' value='skip' disabled><button id='submit-save' name='intent' value='save' type='submit'>Save</button><button id='submit-publish' name='intent' value='publish' type='submit'>Publish</button></form><form id='cancel-form' action='/cancelled' method='post'><button id='cancel-submit' name='intent' value='cancelled' type='submit'>Cancel</button></form><p id='select-output'>idle</p><p id='submit-output'>idle</p></body></html>",
    )?;
    let status = runtime.element_target_by_id("status")?;
    let timer = runtime.element_target_by_id("timer")?;
    let source = runtime.element_target_by_id("source")?;
    let destination = runtime.element_target_by_id("destination")?;
    let removable = runtime.element_target_by_id("removable")?;
    let event_button = runtime.element_target_by_id("event-button")?;
    let event_checkbox = runtime.element_target_by_id("event-checkbox")?;
    let check_checkbox = runtime.element_target_by_id("check-checkbox")?;
    let radio_a = runtime.element_target_by_id("radio-a")?;
    let radio_b = runtime.element_target_by_id("radio-b")?;
    let cancel_radio_a = runtime.element_target_by_id("cancel-radio-a")?;
    let cancel_radio_b = runtime.element_target_by_id("cancel-radio-b")?;
    let event_output = runtime.element_target_by_id("event-output")?;
    let control_output = runtime.element_target_by_id("control-output")?;
    let check_output = runtime.element_target_by_id("check-output")?;
    let text_input = runtime.element_target_by_id("text-input")?;
    let fill_output = runtime.element_target_by_id("fill-output")?;
    let type_input = runtime.element_target_by_id("type-input")?;
    let type_output = runtime.element_target_by_id("type-output")?;
    let disabled_button = runtime.element_target_by_id("disabled-button")?;
    let disabled_input = runtime.element_target_by_id("disabled-input")?;
    let disabled_checkbox = runtime.element_target_by_id("disabled-checkbox")?;
    let disabled_output = runtime.element_target_by_id("disabled-output")?;
    let fixture_form = runtime.element_target_by_id("fixture-form")?;
    let required_name = runtime.element_target_by_id("required-name")?;
    let role_select = runtime.element_target_by_id("role-select")?;
    let tag_select = runtime.element_target_by_id("tag-select")?;
    let terms_checkbox = runtime.element_target_by_id("terms-checkbox")?;
    let submit_save = runtime.element_target_by_id("submit-save")?;
    let submit_publish = runtime.element_target_by_id("submit-publish")?;
    let cancel_form = runtime.element_target_by_id("cancel-form")?;
    let cancel_submit = runtime.element_target_by_id("cancel-submit")?;
    let select_output = runtime.element_target_by_id("select-output")?;
    let submit_output = runtime.element_target_by_id("submit-output")?;

    runtime.evaluate_script(
        r#"
        const first = document.getElementById("status");
        const second = document.getElementById("status");
        if (first !== second) {
            throw new Error("document wrapper identity was not stable");
        }
        Promise.resolve().then(() => { first.textContent = "promise-after"; });
        first.style.setProperty("width", "144px");
        first.style.setProperty("background-color", "rgb(17, 34, 51)");
        const destination = document.getElementById("destination");
        destination.appendChild(document.getElementById("movable"));
        const created = document.createElement("strong");
        created.id = "created";
        created.textContent = "new";
        destination.appendChild(created);
        document.getElementById("removable").remove();
        setTimeout(() => {
            document.getElementById("timer").textContent = "timer-after";
        }, 1);
        const eventButton = document.getElementById("event-button");
        const eventParent = document.getElementById("event-parent");
        const eventOrder = [];
        eventParent.addEventListener("click", function (event) {
            if (event.target !== eventButton) {
                return;
            }
            if (event.eventPhase !== 1 || event.currentTarget !== eventParent ||
                event.composedPath()[0] !== eventButton) {
                throw new Error(`capture event path was incorrect: phase=${event.eventPhase}, current=${event.currentTarget === eventParent}, target=${event.target === eventButton}, targetId=${event.target?.__rwbEngineId}, expectedId=${eventButton.__rwbEngineId}, path=${event.composedPath()[0] === eventButton}, pathId=${event.composedPath()[0]?.__rwbEngineId}`);
            }
            eventOrder.push("capture");
        }, true);
        eventButton.addEventListener("click", function (event) {
            if (event.type !== "click" || event.eventPhase !== 2 ||
                event.target !== eventButton || event.currentTarget !== eventButton ||
                this !== eventButton) {
                throw new Error("target click event identity was incorrect");
            }
            eventOrder.push("target");
            event.preventDefault();
        }, { once: true });
        eventParent.addEventListener("click", function (event) {
            if (event.target !== eventButton) {
                return;
            }
            if (event.eventPhase !== 3 || event.currentTarget !== eventParent) {
                throw new Error("bubble event path was incorrect");
            }
            eventOrder.push("bubble");
            Promise.resolve().then(() => {
                document.getElementById("event-output").textContent = eventOrder.join(">");
            });
        });
        const checkbox = document.getElementById("event-checkbox");
        if (eventButton === checkbox || eventButton.__rwbEngineId !== "event-button" ||
            checkbox.__rwbEngineId !== "event-checkbox") {
            throw new Error(`distinct element wrapper identity was aliased: button=${eventButton.__rwbEngineId}, checkbox=${checkbox.__rwbEngineId}, same=${eventButton === checkbox}`);
        }
        const controlOrder = [];
        for (const type of ["focus", "focusin", "click", "input", "change"]) {
            checkbox.addEventListener(type, () => {
                if ((type === "click" || type === "input" || type === "change") &&
                    checkbox.checked !== true) {
                    throw new Error(`${type} listener did not observe the activated checkbox state`);
                }
                controlOrder.push(type);
                document.getElementById("control-output").textContent = controlOrder.join(">");
            });
        }
        const checkCheckbox = document.getElementById("check-checkbox");
        const checkOrder = [];
        for (const type of ["input", "change"]) {
            checkCheckbox.addEventListener(type, () => {
                checkOrder.push(`${type}:${checkCheckbox.checked}`);
                document.getElementById("check-output").textContent = checkOrder.join(">");
            });
        }
        const radioA = document.getElementById("radio-a");
        const radioB = document.getElementById("radio-b");
        radioB.addEventListener("click", () => {
            if (radioA.checked !== false || radioB.checked !== true) {
                throw new Error("radio click listener did not observe mutually exclusive pre-activated state");
            }
        });
        const cancelRadioA = document.getElementById("cancel-radio-a");
        const cancelRadioB = document.getElementById("cancel-radio-b");
        cancelRadioB.addEventListener("click", (event) => {
            if (cancelRadioA.checked !== false || cancelRadioB.checked !== true) {
                throw new Error("cancelled radio click did not expose pre-activated group state");
            }
            event.preventDefault();
        });
        const typeInput = document.getElementById("type-input");
        const typeOrder = [];
        typeInput.addEventListener("keydown", (event) => {
            typeOrder.push(`down:${event.key}`);
            if (event.key === "!") {
                event.preventDefault();
            }
        });
        typeInput.addEventListener("input", () => {
            typeOrder.push(`input:${typeInput.value}`);
        });
        typeInput.addEventListener("keyup", (event) => {
            typeOrder.push(`up:${event.key}`);
            document.getElementById("type-output").textContent = typeOrder.join(">");
        });
        const textInput = document.getElementById("text-input");
        const fillOrder = [];
        for (const type of ["focus", "focusin", "input", "change"]) {
            textInput.addEventListener(type, () => {
                if ((type === "input" || type === "change") && textInput.value !== "Ada λ") {
                    throw new Error(`${type} listener observed stale value ${textInput.value}`);
                }
                fillOrder.push(type);
                document.getElementById("fill-output").textContent = fillOrder.join(">");
            });
        }
        const disabledOutput = document.getElementById("disabled-output");
        for (const controlId of ["disabled-button", "disabled-input", "disabled-checkbox"]) {
            const control = document.getElementById(controlId);
            for (const type of ["focus", "focusin", "click", "keydown", "keyup", "input", "change"]) {
                control.addEventListener(type, () => {
                    disabledOutput.textContent = `${controlId}:${type}`;
                });
            }
        }
        const selectOutput = document.getElementById("select-output");
        const selectOrder = [];
        for (const controlId of ["role-select", "tag-select"]) {
            const control = document.getElementById(controlId);
            for (const type of ["input", "change"]) {
                control.addEventListener(type, () => {
                    selectOrder.push(`${controlId}:${type}:${control.value}`);
                    selectOutput.textContent = selectOrder.join(">");
                });
            }
        }
        const fixtureForm = document.getElementById("fixture-form");
        fixtureForm.addEventListener("submit", (event) => {
            if (event.target !== fixtureForm || event.currentTarget !== fixtureForm) {
                throw new Error("submit event identity was incorrect");
            }
            document.getElementById("submit-output").textContent = "submitted";
        });
        const cancelForm = document.getElementById("cancel-form");
        cancelForm.addEventListener("submit", (event) => {
            document.getElementById("submit-output").textContent = "cancelled";
            event.preventDefault();
        });
        "#,
    )?;
    ensure!(
        runtime.observe_text(&status)? == "promise-after",
        "semantic observation missed the promise-driven page mutation"
    );
    ensure!(
        runtime.observe_text(&timer)? == "waiting",
        "timer mutation ran before its task checkpoint"
    );
    ensure!(
        runtime.observe_text(&source)? == "source:",
        "appendChild did not detach the live child from its former parent"
    );
    ensure!(
        runtime.observe_text(&destination)? == "destination:movednew",
        "appendChild did not preserve moved and newly created child content"
    );
    let created = runtime.element_target_by_id("created")?;
    ensure!(
        runtime.observe_text(&created)? == "new",
        "document.createElement insertion was not semantically observable"
    );
    ensure!(
        runtime.validate_target(&removable).is_err(),
        "removed page element remained actionable"
    );
    ensure!(
        !runtime.click(&event_button)?,
        "preventDefault did not cancel the click default action"
    );
    ensure!(
        runtime.observe_text(&event_output)? == "capture>target>bubble",
        "capture/target/bubble ordering or post-event promise checkpoint was incorrect"
    );
    ensure!(
        runtime.click(&event_button)?,
        "once click listener was unexpectedly dispatched a second time"
    );
    ensure!(
        runtime.click(&event_checkbox)?,
        "checkbox click default action was unexpectedly cancelled"
    );
    ensure!(
        runtime.observe_checked(&event_checkbox)?,
        "checkbox default action did not update the live control state"
    );
    ensure!(
        runtime.observe_text(&control_output)? == "focus>focusin>click>input>change",
        "focus or checkbox activation events were emitted in the wrong order"
    );
    ensure!(
        runtime.check(&check_checkbox, true)?,
        "check did not report the initial checked-state change"
    );
    ensure!(
        !runtime.check(&check_checkbox, true)?,
        "repeating the requested checked state was not idempotent"
    );
    ensure!(
        runtime.check(&check_checkbox, false)?,
        "check did not report the unchecked-state change"
    );
    ensure!(
        !runtime.observe_checked(&check_checkbox)?,
        "check did not leave the live checkbox in the requested state"
    );
    ensure!(
        runtime.observe_text(&check_output)? == "input:true>change:true>input:false>change:false",
        "check emitted duplicate events or exposed stale checked state"
    );
    ensure!(
        runtime.click(&radio_b)?,
        "radio activation was unexpectedly cancelled"
    );
    ensure!(
        !runtime.observe_checked(&radio_a)? && runtime.observe_checked(&radio_b)?,
        "radio click did not enforce live group mutual exclusion"
    );
    ensure!(
        !runtime.click(&cancel_radio_b)?,
        "cancelled radio activation was reported as default-enabled"
    );
    ensure!(
        runtime.observe_checked(&cancel_radio_a)? && !runtime.observe_checked(&cancel_radio_b)?,
        "preventDefault did not restore the prior radio-group state"
    );
    ensure!(
        runtime.check(&radio_a, true)?,
        "check did not activate a different radio-group member"
    );
    ensure!(
        runtime.observe_checked(&radio_a)? && !runtime.observe_checked(&radio_b)?,
        "radio check did not synchronize peer state"
    );
    ensure!(
        runtime.check(&radio_a, false).is_err(),
        "radio check allowed direct unchecking"
    );
    runtime.focus(&text_input)?;
    runtime.fill(&text_input, "Ada λ")?;
    ensure!(
        runtime.observe_value(&text_input)? == "Ada λ",
        "fill did not update the live text-control value"
    );
    ensure!(
        runtime.observe_text(&fill_output)? == "focus>focusin>input>change",
        "fill focus or editing events were emitted in the wrong order"
    );
    runtime.type_text(&type_input, "Aλ!")?;
    ensure!(
        runtime.observe_value(&type_input)? == "Aλ",
        "cancelled keydown inserted text or Unicode typing lost a character"
    );
    ensure!(
        runtime.observe_text(&type_output)?
            == "down:A>input:A>up:A>down:λ>input:Aλ>up:λ>down:!>up:!",
        "typing exposed the wrong key/input order or live accumulated value"
    );
    ensure!(
        runtime.key(&type_input, "Backspace", &[])?,
        "Backspace default editing was unexpectedly cancelled"
    );
    ensure!(
        runtime.observe_value(&type_input)? == "A",
        "Backspace did not remove the final Unicode scalar from the live editor"
    );
    ensure!(
        runtime.key(&type_input, "ArrowLeft", &[])?,
        "named non-editing key was unexpectedly cancelled"
    );
    ensure!(
        runtime.observe_value(&type_input)? == "A",
        "caret movement unexpectedly changed the live editor"
    );
    ensure!(
        runtime.key(&type_input, "B", &[])?,
        "caret insertion was unexpectedly cancelled"
    );
    ensure!(
        runtime.observe_value(&type_input)? == "BA",
        "caret insertion did not edit before the existing value"
    );
    ensure!(
        !runtime.key(&type_input, "!", &[])?,
        "cancelled keydown was reported as default-enabled"
    );
    ensure!(
        runtime.observe_value(&type_input)? == "BA",
        "cancelled standalone key action changed the live editor"
    );
    ensure!(
        runtime.focus(&disabled_button).is_err(),
        "focus accepted a disabled button"
    );
    ensure!(
        runtime.click(&disabled_button).is_err(),
        "click accepted a disabled button"
    );
    ensure!(
        runtime.fill(&disabled_input, "changed").is_err(),
        "fill accepted a disabled text control"
    );
    ensure!(
        runtime.type_text(&disabled_input, "changed").is_err(),
        "typing accepted a disabled text control"
    );
    ensure!(
        runtime.key(&disabled_input, "x", &[]).is_err(),
        "key accepted a disabled text control"
    );
    ensure!(
        runtime.check(&disabled_checkbox, true).is_err(),
        "check accepted a disabled checkbox"
    );
    ensure!(
        runtime.observe_value(&disabled_input)? == "before",
        "a rejected disabled text-control action changed its live value"
    );
    ensure!(
        !runtime.observe_checked(&disabled_checkbox)?,
        "a rejected disabled checkbox action changed its live state"
    );
    ensure!(
        runtime.observe_text(&disabled_output)? == "idle",
        "a rejected disabled-control action dispatched a page event"
    );

    ensure!(
        runtime.submit(&fixture_form, Some(&submit_save)).is_err(),
        "required empty text control did not block form submission"
    );
    ensure!(
        runtime.observe_text(&submit_output)? == "idle",
        "invalid form submission dispatched a submit event"
    );
    ensure!(
        runtime.select(&role_select, &["editor".into()])?,
        "single-select change was not reported"
    );
    ensure!(
        !runtime.select(&role_select, &["editor".into()])?,
        "repeating a single-select value was not idempotent"
    );
    ensure!(
        runtime.select(&tag_select, &["λ".into(), "rust".into()])?,
        "multi-select change was not reported"
    );
    ensure!(
        runtime.observe_selected_values(&role_select)? == ["editor"],
        "single-select did not retain the requested option"
    );
    ensure!(
        runtime.observe_selected_values(&tag_select)? == ["rust", "λ"],
        "multi-select values were not normalized to document option order"
    );
    ensure!(
        runtime.observe_text(&select_output)?
            == "role-select:input:editor>role-select:change:editor>tag-select:input:rust>tag-select:change:rust",
        "selection events were duplicated, reordered, or exposed stale state"
    );
    runtime.fill(&required_name, "Ada λ")?;
    ensure!(
        runtime.check(&terms_checkbox, true)?,
        "form checkbox did not enter its requested checked state"
    );
    let submission = runtime
        .submit(&fixture_form, Some(&submit_publish))?
        .context("valid form submission was cancelled unexpectedly")?;
    ensure!(
        submission.method == "post",
        "form method was not normalized to post"
    );
    ensure!(
        submission.action == "/submit",
        "form action was not preserved"
    );
    ensure!(
        submission.fields
            == [
                ("name", "Ada λ"),
                ("role", "editor"),
                ("tag", "rust"),
                ("tag", "λ"),
                ("terms", "accepted"),
                ("intent", "publish"),
            ]
            .into_iter()
            .map(|(name, value)| browser_engine::FormField {
                name: name.into(),
                value: value.into(),
            })
            .collect::<Vec<_>>(),
        "successful controls or selected submitter were serialized incorrectly: {:?}",
        submission.fields
    );
    ensure!(
        runtime.observe_text(&submit_output)? == "submitted",
        "valid form submission did not dispatch the page submit event"
    );
    ensure!(
        runtime
            .submit(&cancel_form, Some(&cancel_submit))?
            .is_none(),
        "preventDefault did not cancel form submission"
    );
    ensure!(
        runtime.observe_text(&submit_output)? == "cancelled",
        "cancelled submit event did not remain observable"
    );

    ensure!(runtime.run_next_timer()?, "queued timer did not execute");
    ensure!(
        runtime.observe_text(&timer)? == "timer-after",
        "semantic observation missed the timer-driven page mutation"
    );

    runtime.resolve_layout();
    let pixels = runtime.render_target(&status)?;
    ensure!(
        pixels.width() == 144,
        "JavaScript style mutation did not produce the expected 144px layout width: {}",
        pixels.width()
    );
    let expected_pixel = [17, 34, 51, 255];
    ensure!(
        pixels
            .data()
            .iter()
            .all(|pixel| pixel.to_u8_array() == expected_pixel),
        "Vello CPU pixels did not reflect JavaScript's computed rgb(17, 34, 51) background"
    );

    runtime.evaluate_script(
        "setTimeout(() => { document.getElementById('status').textContent = 'obsolete'; }, 1);",
    )?;
    runtime.navigate(
        "<!doctype html><html><body><main id='status'>replacement</main></body></html>",
    )?;
    ensure!(
        runtime.validate_target(&status).is_err(),
        "navigation did not invalidate a prior document target"
    );
    ensure!(
        !runtime.run_next_timer()?,
        "a timer from the obsolete document survived navigation"
    );
    let replacement = runtime.element_target_by_id("status")?;
    ensure!(
        runtime.observe_text(&replacement)? == "replacement",
        "obsolete callback mutated the replacement document"
    );

    let runaway_error = runtime
        .evaluate_script("while (true) {}")
        .expect_err("runaway page script exceeded the configured loop budget without failing");
    ensure!(
        runaway_error
            .to_string()
            .contains("page script evaluation failed"),
        "runaway script returned an unexpected error: {runaway_error:#}"
    );
    let throwing_error = runtime
        .evaluate_script(
            "document.getElementById('status').textContent = 'changed-before-throw'; throw new Error('boom');",
        )
        .expect_err("throwing page script unexpectedly succeeded");
    ensure!(
        throwing_error
            .to_string()
            .contains("page script evaluation failed"),
        "throwing script did not preserve its evaluation error: {throwing_error:#}"
    );
    ensure!(
        runtime.observe_text(&replacement)? == "changed-before-throw",
        "mutation queued before a script exception was not applied"
    );
    runtime.evaluate_script(
        "document.getElementById('status').textContent = 'recovered-after-runaway';",
    )?;
    ensure!(
        runtime.observe_text(&replacement)? == "recovered-after-runaway",
        "document runtime was not usable after rejecting a runaway page script"
    );

    let inline_throw_runtime = DocumentRuntime::from_html(
        "<!doctype html><html><body><p id='inline-status'>before</p><script>document.getElementById('inline-status').textContent = 'changed'; throw new Error('inline boom');</script></body></html>",
    )?;
    let inline_status = inline_throw_runtime.element_target_by_id("inline-status")?;
    ensure!(
        inline_throw_runtime.observe_text(&inline_status)? == "changed",
        "inline-script mutation was lost when its script threw"
    );

    let database_path = temporary_database_path()?;
    let record = round_trip_component_record(&database_path, "profile-version", b"persisted")?;
    ensure!(
        record.schema_version == COMPONENT_SCHEMA_VERSION,
        "unexpected component schema version: {}",
        record.schema_version
    );
    ensure!(
        record.value == b"persisted",
        "reopened component database returned unexpected bytes"
    );
    fs::remove_file(&database_path).with_context(|| {
        format!(
            "failed to remove temporary component database {}",
            database_path.display()
        )
    })?;

    println!(
        "component-spike: owned Boa realm, stable wrappers, capture/target/bubble event dispatch, checkbox and radio defaults, Unicode editing, selection controls, required-form validation, successful-control serialization with submitter identity, cancelled submission, disabled-control rejection, promise/timer/style/create/move/remove DOM mutations, semantic observation, layout, CPU pixels, navigation cancellation, runaway-script rejection/recovery, and redb reopen succeeded"
    );
    Ok(())
}
