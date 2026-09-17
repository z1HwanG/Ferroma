#!/usr/bin/env node
/**
 * Static validation of the deployment artefacts.
 *
 * `docker build` cannot run on the machine this was written on, so the image is the
 * one part of the delivery that is not exercised by the test suite. This is the next
 * best thing: it checks every precondition the build has, the way a person would by
 * hand, and fails loudly instead of leaving it to be discovered on a server.
 *
 * What it catches — each of these has bitten a real project at least once:
 *
 *   * a new workspace member that the Dockerfile never copies, so the cached build
 *     layer fails with "failed to load manifest for workspace member";
 *   * a member whose placeholder source the Dockerfile forgets to create, so the
 *     dependency-caching layer cannot compile;
 *   * a `COPY` whose source does not exist, or that `.dockerignore` excludes — the
 *     build either fails or silently ships without the file;
 *   * a `COPY --from=builder` path that the build stage never produced, which fails
 *     at the very end of a long build;
 *   * a compose file with a tab (fatal in YAML) or a `${VAR}` with no default and no
 *     `:?` guard, which is how a deployment ends up with an empty secret;
 *   * one `${…}` nested inside another, which Compose does *not* interpolate — the
 *     inner variable silently expands to empty, so `${A:-x:${B}}` becomes `x:`;
 *   * an image published to Docker Hub under a repository the compose files, the
 *     publish script and the workflow disagree about, so an operator pulls a tag
 *     that does not exist;
 *   * a build argument the publish path passes but the Dockerfile never declares,
 *     which BuildKit ignores without a word.
 *
 *   node tools/check-deploy.mjs
 */
import fs from 'node:fs';
import path from 'node:path';

const root = process.cwd();
const problems = [];
const notes = [];

const read = (p) => fs.readFileSync(path.join(root, p), 'utf8');
const exists = (p) => fs.existsSync(path.join(root, p));

/** Members listed in the root `[workspace] members = [...]`. */
function workspaceMembers() {
  const text = read('Cargo.toml');
  const block = text.match(/members\s*=\s*\[([^\]]*)\]/s);
  if (!block) throw new Error('Cargo.toml has no [workspace] members list');
  return [...block[1].matchAll(/"([^"]+)"/g)].map((m) => m[1]);
}

/**
 * Directories excluded by `.dockerignore`, evaluated the way BuildKit does: patterns
 * are matched in order and the *last* match wins, so a `!negation` can bring a path
 * back.
 */
function dockerignoreMatcher() {
  if (!exists('.dockerignore')) return () => false;
  const patterns = read('.dockerignore')
    .split('\n')
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith('#'));

  const toRegex = (pattern) => {
    const negated = pattern.startsWith('!');
    const body = negated ? pattern.slice(1) : pattern;
    let re = '';
    for (let i = 0; i < body.length; i++) {
      const ch = body[i];
      if (ch === '*' && body[i + 1] === '*') {
        re += '.*';
        i++;
        if (body[i + 1] === '/') i++;
      } else if (ch === '*') {
        re += '[^/]*';
      } else if (ch === '?') {
        re += '[^/]';
      } else if ('.+^${}()|[]\\'.includes(ch)) {
        re += '\\' + ch;
      } else {
        re += ch;
      }
    }
    // A bare name matches at any depth; a path with a slash matches from the root.
    const anchored = body.includes('/') ? `^${re}` : `(^|/)${re}`;
    return { negated, regex: new RegExp(`${anchored}($|/)`) };
  };

  const compiled = patterns.map(toRegex);
  return (relativePath) => {
    let ignored = false;
    for (const { negated, regex } of compiled) {
      if (regex.test(relativePath)) ignored = !negated;
    }
    return ignored;
  };
}

function parseDockerfile() {
  const lines = read('Dockerfile').split('\n');
  const stages = [];
  const copies = [];
  let current = null;

  lines.forEach((line, index) => {
    const trimmed = line.trim();
    if (trimmed.startsWith('#')) return;
    const from = trimmed.match(/^FROM\s+\S+\s+AS\s+(\S+)/i);
    if (from) {
      current = from[1];
      stages.push(current);
      return;
    }
    const copy = trimmed.match(/^COPY\s+(.*)$/i);
    if (copy) {
      const parts = copy[1].split(/\s+/).filter(Boolean);
      const fromFlag = parts.findIndex((p) => p.startsWith('--from='));
      const fromStage = fromFlag === -1 ? null : parts[fromFlag].slice('--from='.length);
      const rest = parts.filter((p) => !p.startsWith('--'));
      const sources = rest.slice(0, -1);
      copies.push({ line: index + 1, stage: current, fromStage, sources, dest: rest.at(-1) });
    }
  });

  return { stages, copies };
}

// -----------------------------------------------------------------------------
// A. Every workspace member's manifest is copied, and each crate gets a placeholder
// -----------------------------------------------------------------------------
const members = workspaceMembers();
const dockerfile = read('Dockerfile');
const { stages: dockerStages, copies: dockerfileCopies } = parseDockerfile();
void dockerStages;

notes.push(`workspace members: ${members.length}`);

for (const member of members) {
  const manifest = `${member}/Cargo.toml`;
  if (!exists(manifest)) {
    problems.push(`workspace member ${member} has no Cargo.toml`);
    continue;
  }
  const copied = dockerfileCopies.some(
    (c) =>
      c.stage === 'builder' &&
      !c.fromStage &&
      c.sources.includes(manifest),
  );
  if (!copied) {
    problems.push(
      `Dockerfile never copies ${manifest}; the cached dependency layer will fail with ` +
        `"failed to load manifest for workspace member"`,
    );
  }
}

// Crates (members under crates/) additionally need a placeholder lib.rs, or the
// dependency-caching build has nothing to compile for them.
const crateMembers = members.filter((m) => m.startsWith('crates/'));
const placeholderLoop = dockerfile.match(/for crate in([\s\S]*?);\s*do/);
if (!placeholderLoop) {
  problems.push('Dockerfile has no `for crate in …` placeholder loop');
} else {
  const listed = placeholderLoop[1]
    .replace(/\\/g, ' ')
    .split(/\s+/)
    .filter(Boolean);
  for (const member of crateMembers) {
    const name = path.basename(member);
    if (!listed.includes(name)) {
      problems.push(
        `${name} is a workspace member but the Dockerfile's placeholder loop does not ` +
          `create crates/${name}/src/lib.rs, so the cached build layer cannot compile`,
      );
    }
  }
  for (const name of listed) {
    if (!crateMembers.some((m) => path.basename(m) === name)) {
      notes.push(`Dockerfile placeholder loop lists ${name}, which is not a crate member`);
    }
  }
  notes.push(`placeholder crates: ${listed.length}`);
}

// -----------------------------------------------------------------------------
// B. Every COPY source exists and survives .dockerignore
// -----------------------------------------------------------------------------
const ignored = dockerignoreMatcher();

for (const copy of dockerfileCopies) {
  if (copy.fromStage) continue; // producer paths are checked in section C
  for (const source of copy.sources) {
    if (!exists(source)) {
      problems.push(`Dockerfile:${copy.line} copies ${source}, which does not exist`);
      continue;
    }
    if (ignored(source)) {
      problems.push(
        `Dockerfile:${copy.line} copies ${source}, but .dockerignore excludes it — ` +
          `the build would fail or silently ship without it`,
      );
    }
  }
}

// -----------------------------------------------------------------------------
// C. Every `COPY --from=builder` path is produced by the build stage
// -----------------------------------------------------------------------------
const producedByBuilder = new Set([
  // Written by the placeholder RUN, then replaced by COPY into the build context.
  '/build/config/ferroma.toml',
  '/build/web',
  '/build/admin',
  // Produced by `cargo build --release`.
  '/build/target/release/ferroma',
]);

for (const copy of dockerfileCopies) {
  if (!copy.fromStage) continue;
  for (const source of copy.sources) {
    if (!producedByBuilder.has(source)) {
      problems.push(
        `Dockerfile:${copy.line} copies ${source} from the ${copy.fromStage} stage, but ` +
          `nothing in that stage is known to produce it`,
      );
    }
  }
}

// The build stage must actually build the binary the runtime stage copies.
if (!/cargo build --release --bin ferroma/.test(dockerfile)) {
  problems.push('Dockerfile never runs `cargo build --release --bin ferroma`');
}

// -----------------------------------------------------------------------------
// D. Compose files: no tabs, and every variable is defaulted or guarded
// -----------------------------------------------------------------------------
const composeFiles = fs
  .readdirSync(root)
  .filter((f) => /^docker-compose.*\.ya?ml$/.test(f));

for (const file of composeFiles) {
  const text = read(file);

  if (/\t/.test(text)) {
    problems.push(`${file} contains a tab; YAML forbids tabs for indentation`);
  }

  // Compose does *not* interpolate one `${…}` nested inside another: measured on
  // Compose v5, `${A:-registry/name:${B}}` interpolates to `registry/name:` — the
  // inner variable expands to empty, the `:?` guard inside it never fires, and the
  // resulting reference fails at pull time with "invalid reference format".
  //
  // Comments are stripped first: the compose files explain this trap by quoting the
  // broken form, and a checker that fired on its own documentation would be turned
  // off rather than fixed.
  const code = text
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n');
  if (/\$\{[^}]*\$\{/.test(code)) {
    problems.push(
      `${file} nests one \${…} inside another. Compose does not substitute the inner ` +
        `one — it expands to empty, guard and all — so write two variables side by ` +
        `side instead: "\${FERROMA_REPO:-registry/name}:\${FERROMA_VERSION}"`,
    );
  }

  // An image reference with no `/` names a purely local tag: `docker compose pull`
  // cannot resolve it. Every reference to the application image must carry the
  // registry namespace the release is published under.
  for (const match of text.matchAll(/^\s*image:\s*(.+)$/gm)) {
    const value = match[1].trim();
    if (!/ferroma/i.test(value) || value.includes('/')) continue;
    problems.push(
      `${file} refers to the image "${value}", which has no registry namespace — ` +
        `pulling it would look for a local tag that only exists after a local build`,
    );
  }

  // `${VAR}`, `${VAR:-default}` and `${VAR:?message}` are all fine. A bare `${VAR}`
  // silently expands to empty — *unless* the same variable is guarded with `:?` or
  // given a default with `:-` somewhere else in the file, because Compose interpolates
  // the whole file before it starts anything, so one guard protects every use. That
  // distinction matters: flagging every bare use would push people to sprinkle
  // redundant guards and then ignore the check.
  const guarded = new Set(
    [...text.matchAll(/\$\{([A-Z_][A-Z0-9_]*):[-?]/g)].map((m) => m[1]),
  );
  const bare = [...text.matchAll(/\$\{([A-Z_][A-Z0-9_]*)\}/g)].map((m) => m[1]);
  for (const name of new Set(bare)) {
    if (guarded.has(name)) continue;
    problems.push(
      `${file} uses \${${name}} with no default and no guard anywhere in the file; ` +
        `use \${${name}:-fallback} or \${${name}:?message} so an unset value is caught ` +
        `rather than silently empty`,
    );
  }

  // Every service that mounts a config file should also declare it as a dependency
  // of something — a compose file that references a path it never creates is a
  // deployment that fails on the operator's machine and not on yours.
  const mounts = [...text.matchAll(/-\s+\.\/([^:\s]+):/g)].map((m) => m[1]);
  for (const mount of new Set(mounts)) {
    if (!exists(mount)) {
      notes.push(`${file} mounts ./${mount}, which is not in the repository (the operator supplies it)`);
    }
  }
}

notes.push(`compose files: ${composeFiles.length}`);

// -----------------------------------------------------------------------------
// E. The healthcheck the compose files rely on is a command the binary implements
// -----------------------------------------------------------------------------
for (const file of ['Dockerfile', ...composeFiles]) {
  const text = read(file);
  const health = text.match(/ferroma["'\s,]+healthcheck/);
  if (health) {
    const cli = read('server/src/cli.rs');
    if (!/Healthcheck/.test(cli)) {
      problems.push(`${file} runs \`ferroma healthcheck\`, which the CLI does not define`);
    }
  }
}

// Also: every `ferroma <subcommand>` an operator is *told to run* must exist.
//
// Only fenced code blocks count. Scanning prose and YAML produces nonsense —
// `container_name: ferroma` followed by `restart:` reads as "ferroma restart", and
// "Ferroma is a complete mail system" reads as "ferroma is". A check that cries wolf
// gets ignored, which is worse than not having it.
function fencedBlocks(text) {
  const lines = text.split('\n');
  const blocks = [];
  let current = null;
  for (const line of lines) {
    if (/^\s*```/.test(line)) {
      if (current === null) current = [];
      else {
        blocks.push(current.join('\n'));
        current = null;
      }
      continue;
    }
    if (current !== null) current.push(line);
  }
  // Unclosed fence: treat the remainder as a block rather than dropping it silently.
  if (current && current.length) blocks.push(current.join('\n'));
  return blocks;
}

const cliSource = read('server/src/cli.rs');
const documented = new Set();
for (const file of ['README.md', 'README_zh.md', '.env.example', ...composeFiles, 'Dockerfile']) {
  if (!exists(file)) continue;
  for (const block of fencedBlocks(read(file))) {
    for (const line of block.split('\n')) {
      // A command starts the line (allowing a prompt, indentation, or a path prefix
      // like `./target/release/`). Anything else in the line — an argument, a path,
      // a YAML value — is not a subcommand.
      const match = line.match(
        /^[\s$>#]*(?:docker\s+compose\s+exec\s+\S+\s+)?(?:[\w.@~-]*\/)*ferroma(?:\.exe)?\s+([a-z][a-z-]+)/,
      );
      if (!match) continue;
      // `ferroma` followed by `:` is a YAML key or a path, not a command.
      const after = line.slice(line.indexOf(match[1]) + match[1].length);
      if (after.startsWith(':')) continue;
      documented.add(match[1]);
    }
  }
}
for (const name of documented) {
  const pascal = name
    .split('-')
    .map((part) => part[0].toUpperCase() + part.slice(1))
    .join('');
  if (!new RegExp(`\\b${pascal}\\b`).test(cliSource)) {
    problems.push(
      `"ferroma ${name}" appears in a documented command but no ${pascal} subcommand ` +
        `exists in server/src/cli.rs`,
    );
  }
}
notes.push(`documented subcommands checked: ${[...documented].sort().join(' ')}`);

// -----------------------------------------------------------------------------
// F. The publish path: one repository, one platform list, declared build args
// -----------------------------------------------------------------------------
// Releasing is three files cooperating — `scripts/docker-publish.sh` (a
// maintainer's machine), `.github/workflows/docker-publish.yml` (a tag) and the
// compose files (the operator). They only work if they agree, and nothing about
// editing one of them reminds you to edit the others, so the agreement is checked
// here instead of being rediscovered when a `docker pull` 404s.
const publishScript = 'scripts/docker-publish.sh';
const workflowFile = '.github/workflows/docker-publish.yml';

const scriptText = exists(publishScript) ? read(publishScript) : null;
const workflowText = exists(workflowFile) ? read(workflowFile) : null;

if (scriptText === null) {
  problems.push(`${publishScript} is missing — there is no local way to cut a release`);
} else {
  const declared = scriptText.match(/^DEFAULT_REPO="([^"]+)"/m);
  if (!declared) {
    problems.push(`${publishScript} has no DEFAULT_REPO="…" line to check against`);
  } else {
    const repo = declared[1];

    // Every place that names the published repository has to name the same one.
    const named = [];
    for (const file of composeFiles) {
      for (const m of read(file).matchAll(/FERROMA_REPO:-([^}\s]+)/g)) {
        named.push({ file, repo: m[1] });
      }
    }
    if (workflowText !== null) {
      const m = workflowText.match(/DOCKERHUB_REPO \|\| '([^']+)'/);
      if (!m) {
        problems.push(`${workflowFile} does not default DOCKERHUB_REPO to the published repository`);
      } else {
        named.push({ file: workflowFile, repo: m[1] });
      }
    }
    for (const entry of named) {
      if (entry.repo !== repo) {
        problems.push(
          `${entry.file} publishes from '${entry.repo}' but ${publishScript} pushes to ` +
            `'${repo}' — one of them is stale, and the operator pulls a tag that does not exist`,
        );
      }
    }
    notes.push(`published repository: ${repo} (${named.length + 1} references agree)`);

    // A rolling `latest` is only meaningful if both paths move it under the same
    // conditions; a pre-release moving it is the mistake worth naming.
    if (!/\*-\*\)/.test(scriptText)) {
      problems.push(`${publishScript} does not skip the rolling tags for a pre-release version`);
    }

    // The platform list is duplicated by necessity (shell vs. an action input), so
    // at least the default may not drift.
    const scriptPlatforms = scriptText.match(/^DEFAULT_PLATFORMS="([^"]+)"/m);
    if (scriptPlatforms && workflowText !== null) {
      const workflowPlatforms = workflowText.match(/default: (linux\/\S+)/);
      if (!workflowPlatforms) {
        problems.push(`${workflowFile} has no default platform list to compare with ${publishScript}`);
      } else if (workflowPlatforms[1] !== scriptPlatforms[1]) {
        problems.push(
          `${workflowFile} defaults to ${workflowPlatforms[1]} but ${publishScript} builds ` +
            `${scriptPlatforms[1]} — one release would be missing an architecture`,
        );
      }
    }
  }
}

// Build arguments: the release is labelled from these, and BuildKit reports
// nothing when a `--build-arg` matches no `ARG` — the label is simply absent.
const declaredArgs = new Set(
  [...dockerfile.matchAll(/^ARG ([A-Z_][A-Z0-9_]*)/gm)].map((m) => m[1]),
);
const passedArgs = new Set();
if (scriptText !== null) {
  for (const m of scriptText.matchAll(/--build-arg "([A-Z_][A-Z0-9_]*)=/g)) passedArgs.add(m[1]);
}
if (workflowText !== null) {
  for (const m of workflowText.matchAll(/^\s+([A-Z_][A-Z0-9_]*)=\$\{\{/gm)) passedArgs.add(m[1]);
}
for (const name of passedArgs) {
  if (!declaredArgs.has(name)) {
    problems.push(
      `the publish path passes --build-arg ${name}, but the Dockerfile declares no ` +
        `"ARG ${name}" — BuildKit drops it silently and the label is left empty`,
    );
  }
}
if (passedArgs.size > 0) notes.push(`published build args: ${[...passedArgs].sort().join(' ')}`);

// The Windows entry point has to be a *wrapper*. Windows has no association for
// `.sh`, so `./scripts/docker-publish.sh` from PowerShell is silently a no-op — the
// wrapper is what makes the release runnable on a maintainer's machine at all. It
// must delegate, though: a second implementation would drift from the one CI runs
// and from the compose files this check compares it against.
const wrapper = 'scripts/docker-publish.ps1';
if (scriptText !== null && !exists(wrapper)) {
  problems.push(
    `${wrapper} is missing — on Windows ./${publishScript} prints nothing and ` +
      `publishes nothing, because no file association exists for .sh`,
  );
} else if (exists(wrapper)) {
  // Comments are stripped first, so a note explaining the rule cannot break it.
  const wrapperCode = read(wrapper)
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n');
  if (!wrapperCode.includes('docker-publish.sh')) {
    problems.push(
      `${wrapper} does not name ${publishScript} — it must delegate to the POSIX ` +
        `script rather than reimplement the release`,
    );
  }
  if (/docker\s+buildx\s+build/.test(wrapperCode)) {
    problems.push(
      `${wrapper} runs its own \`docker buildx build\`; the publish logic belongs in ` +
        `${publishScript} alone`,
    );
  }
}

// -----------------------------------------------------------------------------
// Report
// -----------------------------------------------------------------------------
console.log('deployment check');
for (const note of notes) console.log(`  note   ${note}`);
if (problems.length === 0) {
  console.log('  result PASS — no problems');
  process.exit(0);
}
for (const problem of problems) console.log(`  FAIL   ${problem}`);
console.log(`  result FAIL — ${problems.length} problem(s)`);
process.exit(1);
