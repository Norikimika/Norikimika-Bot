process.umask(0o077);
const fs = require('fs');
const path = require('path');
const os = require('os');
const crypto = require('crypto');
const { execFile } = require('child_process');

const SCHEDULE = '0 1 * * *';
const MODE = 'all';
const WORK_DIR = path.join(os.homedir(), 'SERVER', 'Backup');
const LOG_PATH = path.join(WORK_DIR, 'bing.log');
try { fs.chmodSync(LOG_PATH, 0o600); } catch {}

const PRIMARY_PACKAGE = 'com.brave.browser';
const CLONE_PACKAGE = 'com.multibox.clone.hub';
const SEARCH_COUNT = 25;
const URL_BAR_ATTEMPTS = 6;
let workDirReady = false;
let running = false;

function ensureWorkDir() {
  if (workDirReady) return;
  fs.mkdirSync(WORK_DIR, { recursive: true });
  workDirReady = true;
}

function log(message) {
  try {
    ensureWorkDir();
    fs.appendFileSync(LOG_PATH, `[${new Date().toLocaleString()}] ${message}\n`);
  } catch (error) {
    console.error(`Bing rewards log failed: ${error.message}`);
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function run(command, args = [], options = {}) {
  return new Promise((resolve, reject) => {
    execFile(command, args, { cwd: WORK_DIR, ...options }, (error, stdout, stderr) => {
      if (error) {
        error.stdout = stdout;
        error.stderr = stderr;
        reject(error);
        return;
      }
      resolve(stdout);
    });
  });
}

function shell(command, options = {}) {
  return new Promise((resolve, reject) => {
    execFile('su', ['-c', command], { cwd: WORK_DIR, ...options }, (error, stdout, stderr) => {
      if (error) {
        error.stdout = stdout;
        error.stderr = stderr;
        reject(error);
        return;
      }
      resolve(stdout);
    });
  });
}

function randomQuery() {
  return crypto.randomBytes(8).toString('base64').replace(/[^A-Za-z0-9]/g, '').slice(0, 4);
}

function buildSearchUrl(query, previousQuery) {
  const cvid = crypto.randomUUID().toUpperCase();
  return `https://www.bing.com/search?q=${query}&form=QBLH&sp=-1&lq=0&pq=${previousQuery}&sc=12-4&qs=n&sk=&cvid=${cvid}`;
}

function xmlEscape(value) {
  return value.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}

function parseBounds(xml, expression) {
  const match = xml.match(expression);
  if (!match) return null;
  const numbers = match[0].match(/\d+/g)?.map(Number) ?? [];
  if (numbers.length < 4) return null;
  return {
    x: Math.floor((numbers[0] + numbers[2]) / 2),
    y: Math.floor((numbers[1] + numbers[3]) / 2),
  };
}

function findNodeByDescription(xml, description) {
  const escaped = description.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const bounds = '\\[[0-9]+,[0-9]+\\]\\[[0-9]+,[0-9]+\\]';
  const byDesc = parseBounds(xml, new RegExp(`content-desc="${escaped}"[^>]*bounds="${bounds}"`));
  if (byDesc) return byDesc;
  return parseBounds(xml, new RegExp(`text="${escaped}"[^>]*bounds="${bounds}"`));
}

function findUrlBar(xml) {
  const bounds = '\\[[0-9]+,[0-9]+\\]\\[[0-9]+,[0-9]+\\]';
  const byId = parseBounds(
    xml,
    new RegExp(`resource-id="[a-zA-Z0-9_.]*:id/(url_bar|urlbar|omnibox|url_bar_container|search_bar|toolbar_url_box)"[^>]*bounds="${bounds}"`)
  );
  if (byId) return byId;
  return parseBounds(
    xml,
    new RegExp(`content-desc="[^"]*(?:Search or enter|Enter a URL|Enter a web address|Search or type URL|Search Bing|Microsoft Edge)[^"]*"[^>]*bounds="${bounds}"`)
  );
}

async function dumpUi(filename) {
  const target = path.join(WORK_DIR, filename);
  await shell(`uiautomator dump ${target}`);
  return fs.readFileSync(target, 'utf8');
}

async function tap(x, y) {
  await shell(`input tap ${x} ${y}`);
  await sleep(500);
}

async function tapUrlBar(mode) {
  const filename = mode === 'clone' ? 'clone-current.xml' : 'brave-current.xml';
  for (let attempt = 1; attempt <= URL_BAR_ATTEMPTS; attempt++) {
    const xml = await dumpUi(filename);
    const bounds = findUrlBar(xml);
    if (bounds) {
      await tap(bounds.x, bounds.y);
      return;
    }
    log(`url bar not found (${mode}), retry ${attempt}/${URL_BAR_ATTEMPTS}`);
    await sleep(2000);
  }
  throw new Error(`URL bar not found in ${mode} after ${URL_BAR_ATTEMPTS} attempts`);
}

async function typeUrl(url) {
  await shell(`input text '${url.replace(/'/g, "'\\''")}'`);
  await sleep(1000);
  await shell('input keyevent KEYCODE_ENTER');
  await sleep(5000);
}

async function clearUrlBar() {
  await shell('input keycombination 113 29');
  await shell('input keyevent KEYCODE_DEL');
}

async function searchAgain(url) {
  await tapUrlBar(currentMode);
  await clearUrlBar();
  await typeUrl(url);
}

async function openFirstSearch(url, mode) {
  if (mode === 'clone') {
    await shell('am start -W -n com.multibox.clone.hub/.MainActivity');
    await sleep(5000);
    const homeXml = await dumpUi('clone-home.xml');
    const brave = findNodeByDescription(homeXml, 'Brave');
    if (brave) await tap(brave.x, brave.y);
    await sleep(7000);
    await openTabSwitcher();
    const tabsXml = await dumpUi('tabs.xml');
    const newTab = findNodeByDescription(tabsXml, 'New tab');
    if (newTab) await tap(newTab.x, newTab.y);
    await sleep(4000);
    await tapUrlBar('clone');
    await typeUrl(url);
    return;
  }

  await shell(`am start -a android.intent.action.VIEW -d '${url}' ${PRIMARY_PACKAGE}/com.google.android.apps.chrome.Main`);
  await sleep(4000);
}

async function runSearches(mode) {
  let previousQuery = '';
  let lastQuery = '';
  log(`START 25 searches (${mode})`);
  for (let index = 1; index <= SEARCH_COUNT; index++) {
    const query = randomQuery();
    const url = buildSearchUrl(query, previousQuery);
    previousQuery = query;
    lastQuery = query;
    if (index === 1) await openFirstSearch(url, mode);
    else await searchAgain(url);
    log(`search ${index}: ${query}`);
    await sleep(2000);
  }
  log(`DONE searches (${mode})`);
  return lastQuery;
}

async function parseOcrClaim() {
  const image = path.join(WORK_DIR, 'rewards-current.png');
  const output = path.join(WORK_DIR, 'rewards-current');
  await shell(`screencap -p ${image}`);
  await run('tesseract', [image, output, '--psm', '11', 'tsv']);
  const rows = fs.readFileSync(`${output}.tsv`, 'utf8').split('\n').slice(1);
  for (const row of rows) {
    const fields = row.split('\t');
    if (fields[11]?.trim().toLowerCase() === 'claim') {
      return {
        x: Math.floor(Number(fields[6]) + Number(fields[8]) / 2),
        y: Math.floor(Number(fields[7]) + Number(fields[9]) / 2),
      };
    }
  }
  return null;
}

async function claimIfReady() {
  const claim = await parseOcrClaim();
  if (!claim) {
    log('no ready points, skip claim');
    return;
  }
  log(`ready to claim, tapping ${claim.x},${claim.y}`);
  await tap(claim.x, claim.y);
  await sleep(5000);
  const header = await parseOcrClaim();
  if (header) {
    await tap(header.x, header.y + 130);
    log(`confirmed claim at ${header.x},${header.y + 130}`);
  } else {
    await tap(884, 420);
    log('fallback claim 884,420');
  }
  await sleep(4000);
}

async function openTabSwitcher() {
  const xml = await dumpUi('tabs.xml');
  const switcher = findNodeByDescription(xml, 'Show tabs') || findNodeByDescription(xml, 'Tabs');
  if (switcher) {
    await tap(switcher.x, switcher.y);
  } else {
    await tap(766, 2268);
  }
  await sleep(3000);
  return dumpUi('tabs.xml');
}

async function closeLastSearchTab(lastQuery) {
  const xml = await openTabSwitcher();
  const bounds = '\\[[0-9]+,[0-9]+\\]\\[[0-9]+,[0-9]+\\]';

  const dashboard = parseBounds(
    xml,
    new RegExp(`content-desc="Close[^"]*Dashboard[^"]*"[^>]*bounds="${bounds}"`)
  );
  if (dashboard) {
    await tap(dashboard.x, dashboard.y);
    log('closed dashboard tab');
    await sleep(4000);
    return;
  }

  const exact = new RegExp(`content-desc="Close ${xmlEscape(lastQuery)}[^"]*"[^>]*bounds="${bounds}"`);
  let match = xml.match(exact);
  if (!match) match = xml.match(new RegExp(`content-desc="Close[^"]*"[^>]*bounds="${bounds}"`));
  if (match) {
    const numbers = match[0].match(/\d+/g).map(Number);
    await tap(Math.floor((numbers[0] + numbers[2]) / 2), Math.floor((numbers[1] + numbers[3]) / 2));
    log('closed last search tab');
  } else {
    log('no search tab found to close');
  }
  await sleep(2000);
}

async function finishAccount(mode) {
  await shell('input keyevent KEYCODE_BACK');
  await sleep(2000);
  await shell('input keyevent KEYCODE_BACK');
  await sleep(2000);
  await shell('input keyevent KEYCODE_HOME');
  await sleep(2000);
  await shell(`am force-stop ${PRIMARY_PACKAGE}`);
  if (mode === 'clone') await shell(`am force-stop ${CLONE_PACKAGE}`);
}

let currentMode = 'brave';

async function runAccount(mode) {
  currentMode = mode;
  const lastQuery = await runSearches(mode);
  await searchAgain('https://rewards.bing.com/dashboard');
  await sleep(4000);
  await claimIfReady();
  await closeLastSearchTab(lastQuery);
  await finishAccount(mode);
}

async function runRewards() {
  if (running) {
    log('Bing Rewards skipped: previous run still active');
    return;
  }
  running = true;
  log('Bing Rewards started');
  try {
    for (const mode of ['brave', 'clone']) {
      try {
        if (mode === 'clone') await sleep(3000);
        await runAccount(mode);
      } catch (error) {
        log(`Bing Rewards failed (${mode}): ${error.message}`);
        try {
          await shell(`am force-stop ${mode === 'clone' ? CLONE_PACKAGE : PRIMARY_PACKAGE}`);
        } catch {}
      }
    }
    log('DONE all rewards');
  } finally {
    cleanupArtifacts();
    running = false;
  }
}

const ARTIFACT_NAMES = [
  'brave-current.xml',
  'clone-current.xml',
  'clone-home.xml',
  'clone-tabs.xml',
  'tabs.xml',
  'rewards-current.png',
  'rewards-current.tsv',
];

function cleanupArtifacts(directory = WORK_DIR) {
  for (const name of ARTIFACT_NAMES) {
    fs.rmSync(path.join(directory, name), { force: true });
  }
}

function startBingRewards(scheduler) {
  if (!scheduler || typeof scheduler.schedule !== 'function') {
    throw new Error('node-cron scheduler is required');
  }
  scheduler.schedule(SCHEDULE, runRewards, { scheduled: true, recoverMissedExecutions: false });
  log(`Bing Rewards scheduled: ${SCHEDULE}`);
}

module.exports.runRewards = runRewards;
module.exports.SCHEDULE = SCHEDULE;
module.exports.MODE = MODE;
module.exports.WORK_DIR = WORK_DIR;
module.exports.LOG_PATH = LOG_PATH;
module.exports.startBingRewards = startBingRewards;
module.exports.cleanupArtifacts = cleanupArtifacts;
