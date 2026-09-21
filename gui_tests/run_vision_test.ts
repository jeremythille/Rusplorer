/**
 * Run a natural-language, screenshot-based Rusplorer GUI scenario.
 *
 * Deno drives the scenario and calls the Windows UI Automation bridge for
 * native-window clicks and screenshots. Vision evaluation is opt-in.
 *
 * Run with:
 *   deno run --allow-run --allow-read --allow-write --allow-env \
 *     gui_tests/run_vision_test.ts
 * Add --allow-net and --evaluate to call the vision API.
 */

type Step = {
  action: string;
  text?: string;
  value?: string;
  seconds?: number;
  name?: string;
  expect?: string;
};

type Scenario = {
  name: string;
  session?: string;
  steps: Step[];
};

type TestResult = {
  status: "PASS" | "FAIL" | "NOT_RUN";
  step?: number | string;
  screenshot?: string;
  expectation?: string;
  observed?: string;
  reason?: string;
};

const root = new URL("..", import.meta.url);
const rootPath = decodeURIComponent(root.pathname).replace(/^\/[A-Za-z]:/, (m) => m.slice(1));
const testsPath = new URL(".", import.meta.url);
const defaultExe = `${rootPath}rusplorer.exe`;

function option(name: string, fallback: string): string {
  const index = Deno.args.indexOf(name);
  return index >= 0 && Deno.args[index + 1] ? Deno.args[index + 1] : fallback;
}

function hasFlag(name: string): boolean {
  return Deno.args.includes(name);
}

function pathFromArg(path: string): string {
  return path.replace(/^\//, "").replaceAll("/", "\\");
}

function resolvePath(path: string): string {
  const normalized = pathFromArg(path);
  if (/^[A-Za-z]:[\\/]/.test(normalized) || normalized.startsWith("\\\\")) return normalized;
  return `${rootPath}${normalized}`;
}

const defaultScenario = pathFromArg(new URL("selection_scenario.json", testsPath).pathname);
const bridge = pathFromArg(new URL("windows_ui_automation.ps1", testsPath).pathname);
const exe = pathFromArg(option("--exe", defaultExe));
const scenarioPath = pathFromArg(option("--scenario", defaultScenario));
const requestedSession = Deno.args.includes("--session") ? option("--session", "") : "";
const logPath = pathFromArg(option("--log", `${rootPath}rusplorer_error.log`));
const artifactRoot = pathFromArg(option("--artifacts", `${rootPath}gui_tests/artifacts`));
const timeoutSeconds = option("--timeout", "15");
const evaluate = hasFlag("--evaluate");
const keepOpen = hasFlag("--keep-open");
const model = option("--model", Deno.env.get("VISION_MODEL") ?? "gpt-4o-mini");

async function ps(args: string[]): Promise<string> {
  const command = new Deno.Command("powershell.exe", {
    args: ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", bridge, ...args],
    stdout: "piped",
    stderr: "piped",
  });
  const output = await command.output();
  const stdout = new TextDecoder().decode(output.stdout).trim();
  const stderr = new TextDecoder().decode(output.stderr).trim();
  if (!output.success) {
    throw new Error(stderr || stdout || `PowerShell exited with code ${output.code}`);
  }
  return stdout;
}

async function wait(seconds: number): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, seconds * 1000));
}

async function fileState(path: string): Promise<{ mtime: number; size: number } | null> {
  try {
    const info = await Deno.stat(path);
    return { mtime: info.mtime?.getTime() ?? 0, size: info.size };
  } catch {
    return null;
  }
}

async function evaluateScreenshot(imagePath: string, expectation: string): Promise<TestResult> {
  if (!evaluate) return { status: "NOT_RUN", expectation };
  const key = Deno.env.get("OPENAI_API_KEY");
  if (!key) throw new Error("OPENAI_API_KEY is required when --evaluate is used");

  const bytes = await Deno.readFile(imagePath);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  const image = btoa(binary);
  const prompt =
    `You are a visual GUI test evaluator. Inspect the attached screenshot and judge the expectation exactly as a human would. Return JSON only with keys status (PASS or FAIL), observed, and reason. Do not infer anything that is not visible.\n\nExpectation:\n${expectation}`;
  const response = await fetch("https://api.openai.com/v1/chat/completions", {
    method: "POST",
    headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
    body: JSON.stringify({
      model,
      temperature: 0,
      response_format: { type: "json_object" },
      messages: [
        { role: "system", content: "Return valid JSON only." },
        {
          role: "user",
          content: [
            { type: "text", text: prompt },
            { type: "image_url", image_url: { url: `data:image/png;base64,${image}` } },
          ],
        },
      ],
    }),
  });
  if (!response.ok) {
    throw new Error(`Vision API returned ${response.status}: ${await response.text()}`);
  }
  const body = await response.json();
  const content = body.choices?.[0]?.message?.content ?? "{}";
  try {
    const result = JSON.parse(content) as TestResult;
    result.status = String(result.status).toUpperCase() === "PASS" ? "PASS" : "FAIL";
    return result;
  } catch {
    return {
      status: "FAIL",
      observed: content,
      reason: "Vision evaluator did not return valid JSON",
    };
  }
}

async function main(): Promise<number> {
  const scenario = JSON.parse(await Deno.readTextFile(scenarioPath)) as Scenario;
  const session = requestedSession
    ? resolvePath(requestedSession)
    : scenario.session
    ? resolvePath(scenario.session)
    : "";
  const stamp = new Date().toISOString().replaceAll(/[-:TZ.]/g, "").slice(0, 14);
  const artifactDir = `${artifactRoot}\\${scenario.name.replaceAll(/\s+/g, "_")}_${stamp}`;
  await Deno.mkdir(artifactDir, { recursive: true });
  const logBefore = await fileState(logPath);
  let process: Deno.ChildProcess | undefined;
  const results: TestResult[] = [];

  try {
    for (let index = 0; index < scenario.steps.length; index++) {
      const step = scenario.steps[index];
      switch (step.action) {
        case "launch": {
          const args = session ? [session] : [];
          process = new Deno.Command(exe, { args, cwd: exe.replace(/[\\/][^\\/]+$/, "") }).spawn();
          break;
        }
        case "wait":
          await ps(["-Action", "wait", "-TimeoutSeconds", timeoutSeconds]);
          await wait(step.seconds ?? 1);
          break;
        case "click_text":
          await ps(["-Action", "click_text", "-Text", step.text ?? ""]);
          break;
        case "ctrl_click_text":
          await ps(["-Action", "ctrl_click_text", "-Text", step.text ?? ""]);
          break;
        case "double_click_text":
          await ps(["-Action", "double_click_text", "-Text", step.text ?? ""]);
          break;
        case "key":
          await ps(["-Action", "key", "-Key", step.value ?? ""]);
          break;
        case "screenshot": {
          const imagePath = `${artifactDir}\\${step.name ?? `step_${index + 1}`}.png`;
          await ps([
            "-Action",
            "screenshot",
            "-Path",
            imagePath,
            "-TimeoutSeconds",
            timeoutSeconds,
          ]);
          const result = await evaluateScreenshot(
            imagePath,
            step.expect ?? "Inspect the screenshot for unexpected changes.",
          );
          result.step = index + 1;
          result.screenshot = imagePath;
          results.push(result);
          console.log(`[${result.status}] ${step.expect ?? "screenshot"}`);
          if (result.status === "FAIL") console.log(result.reason ?? "");
          break;
        }
        default:
          throw new Error(`Unknown action: ${step.action}`);
      }
    }

    const logAfter = await fileState(logPath);
    if (JSON.stringify(logAfter) !== JSON.stringify(logBefore)) {
      results.push({ status: "FAIL", step: "crash-log", reason: `Crash log changed: ${logPath}` });
    }
  } finally {
    if (process && !keepOpen) {
      try {
        process.kill("SIGTERM");
      } catch {
        // Process may already have exited.
      }
    }
    await Deno.writeTextFile(`${artifactDir}\\report.json`, JSON.stringify(results, null, 2));
    console.log(`Artifacts: ${artifactDir}`);
  }

  return results.some((result) => result.status === "FAIL") ? 1 : 0;
}

if (import.meta.main) Deno.exit(await main());
