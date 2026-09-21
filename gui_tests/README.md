# Rusplorer vision-based GUI tests

These tests drive the real Windows application, capture what the user sees, and optionally ask a vision model to judge a natural-language expectation.

## Run

Install Deno from https://deno.com/ and run from the repository root:

```powershell
deno run --allow-run --allow-read --allow-write --allow-env gui_tests/run_vision_test.ts
```

The runner uses `windows_ui_automation.ps1` for native Windows UI Automation. Without `--evaluate`, it captures screenshots and writes `report.json` under `gui_tests/artifacts/` without making a network request.

To evaluate screenshots with a vision model:

```powershell
$env:OPENAI_API_KEY = "paste-key-here"
deno run --allow-run --allow-read --allow-write --allow-env --allow-net gui_tests/run_vision_test.ts --evaluate
```

The key is read from the environment and is never stored in this repository. Use `VISION_MODEL` or `--model` to select another compatible vision model.

## Options

- `--exe C:\path\rusplorer.exe`
- `--session C:\path\test.rsess`
- `--scenario gui_tests\selection_scenario.json`
- `--evaluate`
- `--keep-open`
- `--model gpt-4o-mini`

The default selection scenario uses the tracked fixture in `gui_tests/fixture/`
through `gui_tests/selection_fixture.rsess`, so it does not depend on a personal
folder or the application's saved configuration.

## Write a scenario

Edit `selection_scenario.json`. Supported actions:

- `launch`
- `wait` with `seconds`
- `click_text` with `text`
- `ctrl_click_text` with `text`
- `double_click_text` with `text`
- `key` with a Windows SendKeys value, for example `{ESC}`
- `screenshot` with `name` and natural-language `expect`

The evaluator receives the screenshot and the `expect` sentence. It returns `PASS` or `FAIL`, an observation, and a reason. Every result is saved with its screenshot.

Use `--keep-open` while calibrating a scenario. Do not use a real personal folder for destructive actions; create a temporary fixture folder instead.

## Why AccessKit is enabled

The application enables eframe's `accesskit` feature so the PowerShell UI Automation bridge can identify egui controls by their visible names. The vision check still judges the screenshot, so it tests the rendered result rather than only internal widget state.
