// Visual-verification harness. Drives the app in headless chromium and captures the key screens
// in light + dark. Usage: node scripts/shot.mjs [outDir]
import { chromium } from "playwright";
import { mkdirSync } from "node:fs";

const BASE = process.env.SHOT_BASE || "http://localhost:5930";
const outDir = process.argv[2] || "/tmp/hull-shots";
mkdirSync(outDir, { recursive: true });

const shot = async (page, name) => {
  await page.waitForTimeout(700);
  await page.screenshot({ path: `${outDir}/${name}.png`, fullPage: true });
  console.log("shot:", name);
};

const run = async () => {
  const browser = await chromium.launch();
  for (const theme of ["light", "dark"]) {
    const ctx = await browser.newContext({ viewport: { width: 1400, height: 1000 }, deviceScaleFactor: 2 });
    const page = await ctx.newPage();
    await page.addInitScript((t) => localStorage.setItem("hull_theme", t), theme);
    // home
    await page.goto(`${BASE}/?tenant=tankrap`, { waitUntil: "networkidle" });
    await page.waitForTimeout(1200);
    await shot(page, `home-${theme}`);
    // sign in as demo (best-effort)
    try { await page.getByText("demo", { exact: true }).first().click({ timeout: 2000 }); await page.waitForTimeout(800); } catch {}
    // open a repo (first flat row in the Repositories panel)
    try { await page.locator(".rows .row").first().click({ timeout: 3000 }); await page.waitForTimeout(1200); await shot(page, `repo-${theme}`); } catch { console.log("no repo to open"); }
    // PRs tab
    try { await page.getByText(/pull requests/i).first().click({ timeout: 2000 }); await page.waitForTimeout(1000); await shot(page, `prs-${theme}`); } catch {}
    // expand first PR then open its review
    try { await page.locator(".panel .rows .row").first().click({ timeout: 2000 }); await page.waitForTimeout(600);
          await page.locator(".review-row").first().click({ timeout: 2500 }); await page.waitForTimeout(1600); await shot(page, `review-${theme}`); } catch {}
    await ctx.close();
  }
  await browser.close();
};
run().then(() => console.log("done")).catch((e) => { console.error(e); process.exit(1); });
