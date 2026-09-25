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
 *   * a static file that is not world-readable (`0600` is what some editors write),
 *     which ships unreadable and turns the Webmail and Admin into a blank page;
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
// B2. What the image ships as static files must be readable by uid 10001
// -----------------------------------------------------------------------------
// `COPY` preserves the mode of the file it copies, and the service runs as uid 10001. A
// source file that is not world-readable therefore ships unreadable: the static file
// server answers 404, the front-end's ES module graph fails to load, and the Webmail and
// Admin render a blank page — a symptom with nothing in the server log to connect it to
// a file mode. 0.1.4 shipped that way (six files at 0600) and 0.1.3 had it too.
//
// The Dockerfile now normalises what it ships, so this is a hazard rather than a broken
// image. It is checked anyway: a deployment that bind-mounts a checkout instead of using
// the image (`FERROMA__API__WEBMAIL_DIR`) gets no protection from the Dockerfile, and the
// mode of a file is invisible in review — git records only the exec bit.
for (const tree of ['web', 'admin', 'shared', 'config']) {
  if (!exists(tree)) continue;
  for (const entry of fs.readdirSync(path.join(root, tree), { recursive: true })) {
    const rel = path.join(tree, String(entry));
    const stat = fs.statSync(path.join(root, rel));
    if (!stat.isFile()) continue;
    if ((stat.mode & 0o004) === 0) {
      problems.push(
        `${rel} is not world-readable (mode 0${(stat.mode & 0o777).toString(8)}) — ` +
          `\`COPY\` preserves that mode, so uid 10001 reads the file as a 404 and a ` +
          `front-end that imports it never boots`,
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
  '/build/shared',
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
  // Comments are stripped first, and so are quoted healthcheck URLs: the production
  // file documents nested-interpolation traps in comments and probes the API with
  // shell-expanded `$${VAR}` forms that are not Compose interpolations at all. A
  // checker that fired on its own documentation — or on a shell variable — would be
  // turned off rather than fixed.
  const code = text
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n')
    .replace(/\$\$/g, '');
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

  // A service with both `build:` and `image:` is not "build it and call it that" to
  // Compose: with the default pull policy it tries to *pull* the name first. Measured
  // on Compose v5 against `docker-compose.yml`, `docker compose up -d` pulled
  // `wesukilaye/ferroma:dev` — a tag that was never published — and stopped there
  // instead of building the tree, which is the opposite of what the file, its comment
  // and the README all say it does. Such a service has to pin `pull_policy: build`.
  {
    const services = [];
    let inServices = false;
    let current = null;
    for (const line of code.split('\n')) {
      if (/^services:\s*$/.test(line)) {
        inServices = true;
        continue;
      }
      if (!inServices) continue;
      // A key in column zero ends the `services:` block.
      if (/^\S/.test(line)) {
        inServices = false;
        current = null;
        continue;
      }
      const name = /^ {2}([A-Za-z0-9._-]+):\s*$/.exec(line);
      if (name) {
        current = { name: name[1], build: false, image: false, pullPolicy: false };
        services.push(current);
        continue;
      }
      if (!current) continue;
      if (/^ {4}build:/.test(line)) current.build = true;
      if (/^ {4}image:/.test(line)) current.image = true;
      if (/^ {4}pull_policy:/.test(line)) current.pullPolicy = true;
    }
    for (const service of services) {
      if (service.build && service.image && !service.pullPolicy) {
        problems.push(
          `${file}: the "${service.name}" service both builds and names an image, ` +
            `which Compose reads as "pull that name first" — add \`pull_policy: build\` ` +
            `so it builds the tree instead of looking for a tag that may not exist`,
        );
      }
    }
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

// -----------------------------------------------------------------------------
// G. The production compose file applies what the deployment script promises
// -----------------------------------------------------------------------------
// `docker-compose.yml` is the file `scripts/deploy.sh` drives: its `.env` is that
// script's output, and its container has to receive it. The last time the two
// drifted, the generated database credentials and the loopback API bind never
// reached the container — the API served 0.0.0.0:8080 instead of 127.0.0.1:18080
// and the deployment could not connect to its own database.
{
  const deployEnv = read('scripts/deploy.sh');
  const prod = read('docker-compose.yml');
  const prodCode = prod
    .split('\n')
    .filter((line) => !/^\s*#/.test(line))
    .join('\n');

  // The `.env` the script writes is what the container reads.
  if (!/^\s*env_file:\s*$/m.test(prodCode) || !/^\s*-\s*\.env\s*$/m.test(prodCode)) {
    problems.push(
      'docker-compose.yml does not read `.env` (env_file): the values ' +
        '`scripts/deploy.sh` writes would never reach the container',
    );
  }

  // Every name `write_env()` persists must cross into the container. `env_set X`
  // is the write; `X:` / `${X…}` in the service environment is the handover.
  const written = new Set(
    [...deployEnv.matchAll(/^\s*env_set\s+([A-Z_][A-Z0-9_]*)/gm)].map((m) => m[1]),
  );
  const bookkeeping = new Set([
    'POSTGRES_USER',
    'POSTGRES_PASSWORD',
    'POSTGRES_DB',
    'DB_HOST',
    'DB_PORT',
    'POSTGRES_IMAGE',
    'FERROMA_IMAGE',
    'FERROMA_DEPLOY_DOMAIN',
    'FERROMA_DEPLOY_ADMIN',
    'FERROMA_DEPLOY_SETUP_DONE',
    'WEB_PORT',
  ]);
  const forwarded = new Set([
    ...[...prodCode.matchAll(/^\s*([A-Z_][A-Z0-9_]*):/gm)].map((m) => m[1]),
    ...[...prodCode.matchAll(/\$\{([A-Z_][A-Z0-9_]*)(?::|[\s}])/g)].map((m) => m[1]),
  ]);
  for (const name of [...written].sort()) {
    if (bookkeeping.has(name)) continue;
    // `FERROMA_PUBLIC_URL` reaches the server as the alias it reads.
    if (name === 'FERROMA_PUBLIC_URL' && forwarded.has('FERROMA_API_PUBLIC_URL')) continue;
    if (!forwarded.has(name)) {
      problems.push(
        `docker-compose.yml never forwards ${name}, but scripts/deploy.sh writes it ` +
          `into .env — the deployment would silently keep its default`,
      );
    }
  }

  // The database, identity and token secret must be present, not defaulted away.
  for (const name of ['DATABASE_URL', 'FERROMA_HOSTNAME', 'FERROMA_PUBLIC_URL', 'FERROMA_JWT_SECRET']) {
    if (!new RegExp(`\\$\\{${name}:\\?`).test(prodCode)) {
      problems.push(
        `docker-compose.yml must guard ${name} with \${${name}:?…}: without it an ` +
          `unset value boots on an insecure default instead of failing loudly`,
      );
    }
  }

  // The production stack mounts no file-based configuration: with `.env` as the
  // whole configuration, a mounted `ferroma.toml` would silently compete with it.
  if (/-\s+\.\/config\/ferroma\.toml:/.test(prodCode)) {
    problems.push(
      'docker-compose.yml mounts ./config/ferroma.toml: the production stack takes ' +
        'its configuration from .env, and a mounted file would compete with it',
    );
  }

  // The health check must follow the configured bind, not a hard-coded address.
  const hardcoded = [...prodCode.matchAll(/healthcheck[\s\S]{0,400}?127\.0\.0\.1:8080/g)];
  if (hardcoded.length > 0) {
    problems.push(
      'docker-compose.yml probes a hard-coded 127.0.0.1:8080: the production ' +
        'health check must follow FERROMA_API_HOST:FERROMA_API_PORT',
    );
  }
  if (!/FERROMA_API_HOST/.test(prodCode) || !/FERROMA_API_PORT/.test(prodCode)) {
    problems.push('docker-compose.yml must pass FERROMA_API_HOST and FERROMA_API_PORT to the health check');
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
// Releasing is the local script and the compose file the operator pulls from.
// There is no GitHub workflow: a tag push does not build or push an image, and
// `scripts/docker-publish.sh` is the only path that does. The two still have to
// name the same repository, or a `docker pull` 404s.
const publishScript = 'scripts/docker-publish.sh';

const scriptText = exists(publishScript) ? read(publishScript) : null;

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
// must delegate, though: a second implementation would drift from the script this
// check compares against the compose files.
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
