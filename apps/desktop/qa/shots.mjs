// QA: drive the built UI (dist/) against the mock backend and save
// screenshots + a report. Run: node qa/shots.mjs  (needs `playwright`).
import { chromium } from "playwright";
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const dist = path.resolve(here, "../dist");
const out = path.resolve(here, "../qa-out");
fs.mkdirSync(out, { recursive: true });

const types = { ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".svg": "image/svg+xml", ".png": "image/png", ".woff2": "font/woff2", ".json": "application/json" };
const server = http.createServer((req, res) => {
  const u = decodeURIComponent((req.url || "/").split("?")[0]);
  let f = path.join(dist, u === "/" ? "index.html" : u);
  if (!f.startsWith(dist) || !fs.existsSync(f)) f = path.join(dist, "index.html");
  res.writeHead(200, { "content-type": types[path.extname(f)] || "application/octet-stream" });
  fs.createReadStream(f).pipe(res);
});
await new Promise((r) => server.listen(4173, r));

const report = { checks: [], errors: [] };
const check = (name, ok, detail = "") => {
  report.checks.push({ name, ok: !!ok, detail });
  console.log(`${ok ? "PASS" : "FAIL"}  ${name}${detail ? " — " + detail : ""}`);
};

const browser = await chromium.launch();
const page = await browser.newPage({ viewport: { width: 1600, height: 900 }, deviceScaleFactor: 1 });
page.on("pageerror", (e) => report.errors.push(String(e)));
page.on("console", (m) => m.type() === "error" && report.errors.push("console: " + m.text()));
await page.addInitScript({ path: path.join(here, "mock-tauri.js") });
await page.goto("http://localhost:4173/");
await page.waitForSelector(".source-strip .src-tile", { timeout: 15000 });
await page.waitForTimeout(800);
const shot = (n) => page.screenshot({ path: path.join(out, n + ".png") });
await shot("01-workspace");

const visibleText = async () => (await page.evaluate(() => document.body.innerText)).length;

// 1. Audio processing tab: must not blank the window.
await page.getByRole("tab", { name: "Audio processing" }).click();
await page.waitForTimeout(600);
await shot("02-audio-processing-tab");
check("Audio processing tab keeps the UI alive", (await page.locator(".topbar").count()) === 1 && (await visibleText()) > 200 && (await page.locator(".fx-editor").count()) === 1);
check("No React crash box after Audio processing", (await page.locator(".panel-crash").count()) === 0);

// 2. Toggle an effect from the tab: the mock backend must receive it.
const before = (await page.evaluate(() => window.__qaCalls.length));
await page.locator(".fx", { hasText: "Low cut" }).locator(".fx-switch").first().click();
await page.waitForTimeout(250);
const fxCall = await page.evaluate((n) => window.__qaCalls.slice(n).find((c) => c[0] === "mixer_set_fx" || c[0] === "mixer_set_master_fx"), before);
check("Effect switch reaches the backend", !!fxCall && fxCall[1].fx.highpass_enabled === true, JSON.stringify(fxCall?.[0]));

// 3. Mixer audio settings window.
await page.getByRole("button", { name: "Audio settings", exact: true }).click();
await page.waitForSelector(".fx-modal");
await page.waitForTimeout(500);
await shot("03-mixer-audio-settings");
check("Audio settings window lists channels + master", (await page.locator(".fx-modal .kind-list button").count()) >= 3);
await page.locator(".fx-modal .kind-list button", { hasText: "Master" }).click();
await page.waitForTimeout(300);
await shot("04-master-effects");
check("Master shows the limiter on", await page.locator(".fx-modal .fx.on", { hasText: "Limiter" }).count() === 1);
await page.getByRole("button", { name: "Done" }).click();

// 4. Delete a source block (two clicks).
const tilesBefore = await page.locator(".src-tile").count();
const tp = page.locator(".src-tile", { hasText: "Test Pattern" });
await tp.locator(".icon-btn.del").click();
await page.waitForTimeout(150);
await shot("05-delete-armed");
check("First click arms delete", (await tp.locator(".icon-btn.del.armed").count()) === 1);
await tp.locator(".icon-btn.del").click();
await page.waitForTimeout(600);
check("Second click deletes the source", (await page.locator(".src-tile").count()) === tilesBefore - 1);

// 5. Drag a block to reorder (pointer drag).
const names = async () => page.locator(".src-tile .src-name").allInnerTexts();
const n0 = await names();
const a = await page.locator(".src-tile", { hasText: "Camera" }).boundingBox();
const b = await page.locator(".src-tile", { hasText: "Color #0073ff" }).boundingBox();
await page.mouse.move(a.x + 40, a.y + a.height / 2);
await page.mouse.down();
await page.mouse.move(a.x + 80, a.y + a.height / 2, { steps: 4 });
await page.mouse.move(b.x + 20, b.y + b.height / 2, { steps: 8 });
await page.waitForTimeout(100);
await shot("06-dragging");
await page.mouse.up();
await page.waitForTimeout(600);
const n1 = await names();
check("Drag moves the block", n1.indexOf("Camera") === 0 && n0.indexOf("Camera") > 0, `${n0.join(" | ")}  →  ${n1.join(" | ")}`);
const reorderCall = await page.evaluate(() => window.__qaCalls.filter((c) => c[0] === "source_reorder").pop());
check("Reorder sent to backend with every source", !!reorderCall && reorderCall[1].order.length === 5, JSON.stringify(reorderCall?.[1]?.order));
await shot("07-after-reorder");

// 6. Output → Video format: six resolutions, custom bitrate.
const fmtTab = page.getByRole("tab", { name: "Video format" });
if (await fmtTab.count()) await fmtTab.first().click();
else {
  await page.getByRole("tab", { name: "Output" }).click();
  await page.getByRole("tab", { name: "Video format" }).click();
}
await page.waitForTimeout(400);
await shot("08-video-format");
const res = await page.locator(".res-grid button").allInnerTexts();
check("Six output resolutions", res.length === 6 && ["360p", "480p", "540p", "720p", "1080p", "1440p"].every((r) => res.some((x) => x.includes(r))), res.join(","));
await page.locator(".res-grid button", { hasText: "1440p" }).click();
await page.waitForTimeout(300);
check("1440p sent to backend", await page.evaluate(() => window.__qaState.video.output_width === 2560 && window.__qaState.video.output_height === 1440));
const br = page.getByLabel("Custom bitrate");
await br.fill("12345");
await br.press("Enter");
await page.waitForTimeout(300);
check("Custom bitrate applied", await page.evaluate(() => window.__qaState.video.bitrate_kbps === 12345));
await page.locator(".res-grid button", { hasText: "360p" }).click();
await page.waitForTimeout(300);
await shot("09-video-format-360p");
check("Bitrate warning for 360p at 12345 kbps", (await page.locator(".bitrate-note").count()) === 1);

// 7. Video input window.
await page.getByRole("button", { name: "Video input", exact: true }).click();
await page.waitForSelector(".vi-modal");
await page.waitForTimeout(700);
await shot("10-video-input");
const sizes = await page.locator(".vi-form select").nth(1).locator("option").allInnerTexts();
check("Video input lists the camera's own sizes", sizes.some((s) => s.includes("1920×1080")) && sizes.some((s) => s.includes("640×360")), sizes.join(" / "));
await page.getByRole("radio", { name: "High" }).click();
await page.waitForTimeout(200);
await page.getByRole("button", { name: "Apply to camera" }).click();
await page.waitForTimeout(700);
await shot("11-video-input-applied");
const cfg = await page.evaluate(() => window.__qaCalls.filter((c) => c[0] === "camera_configure").pop());
check("High preset asks the camera for 1920×1080", !!cfg && cfg[1].width === 1920 && cfg[1].height === 1080, JSON.stringify(cfg?.[1]));
await page.locator(".vi-form select").first().selectOption("USB Capture HDMI").catch(() => {});
await page.waitForTimeout(600);
await shot("12-video-input-capture-card");
await page.getByRole("button", { name: "Done" }).click();

check("No page errors", report.errors.length === 0, report.errors.slice(0, 5).join(" || "));
fs.writeFileSync(path.join(out, "report.json"), JSON.stringify(report, null, 2));
await browser.close();
server.close();
const failed = report.checks.filter((c) => !c.ok).length;
console.log(`\n${report.checks.length - failed}/${report.checks.length} checks passed`);
process.exit(failed ? 1 : 0);
