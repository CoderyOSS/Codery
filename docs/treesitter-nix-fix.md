# Treesitter Highlighting in NvChad (Apps Container)

Symptom: Elixir (or any language) shows **no syntax highlighting** in nvim inside the
apps container, despite `home.nix` declaring `nvim-treesitter.withAllGrammars`.

This guide documents the root cause, the fix that's baked into the Dockerfile, and
how to diagnose / repair if it ever breaks again.

---

## TL;DR — How it's wired today

`containers/apps/Dockerfile` contains a `RUN` block (after the NvChad clone step)
that symlinks nix-built parser `.so`s and nix-shipped query `.scm`s into
`/home/gem/.local/share/nvim/site/{parser,queries}/` — which is on nvim's
`runtimepath` by default. This bypasses the broken `nvim-treesitter` installer
entirely.

```dockerfile
RUN SITE=/home/gem/.local/share/nvim/site && \
    mkdir -p "$SITE/parser" "$SITE/queries" && \
    for so in /nix/store/*-vimplugin-treesitter-grammar-*/parser/*.so; do \
      ...
    done && \
    TS_NIX=$(ls -d /nix/store/*-vimplugin-nvim-treesitter-2* 2>/dev/null | head -1) && \
    for q in "$TS_NIX"/queries/*; do \
      ...
    done
```

If highlighting breaks in the future, jump to [Diagnosis](#diagnosis).

---

## The three-layer problem

This was not a config preference issue. Three independent failures combined:

### 1. Version skew (the trigger)

`lazy-lock.json` pinned `nvim-treesitter` to a recent master commit that uses
`vim.list.unique` / `vim.list.extend` — Lua APIs added in **neovim 0.12+**. The apps
container ships **neovim 0.11.5** from `nixos-25.05`.

Result: `vim.list` is `nil` at runtime, so
`require("nvim-treesitter").install(...)` crashes with
`attempt to index field 'list' (a nil value)` at `config.lua:171` (`norm_languages`).
`:TSInstall`, `:TSInstallSync`, and NvChad's default `ensure_installed = { "lua",
"luadoc", ... }` **all fail** — for **every language**, not just Elixir. The
errors are silent (pcall-wrapped by NvChad's autocmds).

### 2. Parsers unreachable

`containers/apps/home.nix:31` declares `nvim-treesitter.withAllGrammars`. Nix dutifully
builds `elixir.so`, `heex.so`, `eex.so`, and ~100 more into the nix store, e.g.:

```
/nix/store/14k1fs5x66s4v5sz27fa7fyg81y65gfj-vimplugin-treesitter-grammar-elixir/parser/elixir.so
/nix/store/5li5vyjizgclbza8hi6v32zbs9vbbqbb-...-eex/parser/eex.so
/nix/store/axn3vmc4pnxqw6mmmmc7gl5j60yvxkks-...-heex/parser/heex.so
```

But lazy.nvim loads its **own** copy of `nvim-treesitter` into `&runtimepath`,
shadowing the nix-installed one. The nix parsers exist on disk but nvim never
sees them because they're not on any path nvim scans for parsers.

### 3. Queries unreachable

The treesitter plugin stores query files (`.scm` — highlight capture rules) at
`<plugin>/runtime/queries/{lang}/`. Neovim's treesitter engine scans
`<rtp>/queries/{lang}/` — **one directory level shallower**.

The installer normally bridges this gap by symlinking each language's queries
into `stdpath('data')/site/queries/{lang}/`. With the installer broken (problem
#1), no bridge is built, so even when a parser loads, nvim has no captures to
paint with — highlighting silently does nothing.

---

## The fix

Bypass the broken installer entirely. Symlink the two artifacts nvim's treesitter
engine needs directly into the default `runtimepath` location
(`stdpath('data')/site/` = `~/.local/share/nvim/site/`):

- **Parsers** (`parser/{lang}.so`) ← from nix-built grammars
  (`/nix/store/*-vimplugin-treesitter-grammar-*/parser/*.so`)
- **Queries** (`queries/{lang}/`) ← from nix's `vimPlugins.nvim-treesitter`
  (`/nix/store/*-vimplugin-nvim-treesitter-*/queries/`)

### Why nix's treesitter queries, not lazy's?

Lazy's `runtime/queries/` doesn't exist at Dockerfile build time. Lazy bootstraps on
first interactive nvim launch, deferred intentionally (see Dockerfile comment near
the NvChad clone step — avoids lazy.nvim forges rate-limiting the build). The nix
copy of `vimPlugins.nvim-treesitter` is materialized at build time and has
`queries/` at the **top level** (rtp-friendly layout).

Bonus: the nix version (e.g. dated 2025-04-30 at time of writing) predates the
`vim.list` API breakage, so it's a working plugin version too.

---

## Diagnosis (when highlighting breaks again)

### Quick check — does it work right now?

```bash
ssh gem@apps 'cd /tmp && printf "defmodule F do\n  def g, do: :ok\nend\n" > t.ex && \
  nvim --headless -c "edit t.ex" \
    -c "lua local ok,_=pcall(vim.treesitter.get_parser,0); print(\"parser=\"..tostring(ok)); print(\"query=\"..tostring(vim.treesitter.query.get(\"elixir\",\"highlights\")~=nil))" \
    -c "qa" 2>&1 | tail -5'
```

Expected output:
```
parser=true
query=true
```

If both true → highlighting works. If colors still don't render interactively,
the issue is the colorscheme, not treesitter (check `:TSBufHighlight` and
`:hi @keyword.elixir`).

### If `parser=false`

Parsers are missing. Check what's symlinked:

```bash
ssh gem@apps 'ls /home/gem/.local/share/nvim/site/parser/ | head -20'
```

Should list `.so` files for ~100 languages (elixir, heex, eex, lua, vim, rust,
python, etc.).

If empty / wrong:
- The Dockerfile `RUN` block at the top of this file is missing or broken.
- Or the glob `/nix/store/*-vimplugin-treesitter-grammar-*/parser/*.so` matched
  nothing — `home.nix` no longer has `nvim-treesitter.withAllGrammars`.
- Rebuild the apps container after fixing.

### If `query=false`

Parsers loaded but no query rules. Check what's symlinked:

```bash
ssh gem@apps 'ls /home/gem/.local/share/nvim/site/queries/'
```

Should list language directories including `elixir`, `heex`, `eex`.

If missing:
- The Dockerfile `RUN` block's queries-glob didn't match.
- Check that the nix store path still exists:
  ```bash
  ssh gem@apps 'ls -d /nix/store/*-vimplugin-nvim-treesitter-* 2>/dev/null'
  ```
- If nix changed the package name, update the glob in the Dockerfile.

### If both true but interactive nvim still monochrome

The FileType autocmd that starts treesitter didn't fire. Check NvChad's autocmds:
`~/.local/share/nvim/lazy/NvChad/lua/nvchad/autocmds.lua` should contain
`pcall(vim.treesitter.start)` on `FileType`. If you've overridden user autocmds,
make sure you didn't disable it.

Manual test inside nvim: open a `.ex` file and run `:TSEnable highlight` (or
`:lua vim.treesitter.start()`).

---

## Manual / ad-hoc fix (without rebuilding container)

If you need highlighting working immediately and can't wait for a full rebuild:

```bash
ssh gem@apps 'bash -s' <<'EOF'
SITE=/home/gem/.local/share/nvim/site
mkdir -p "$SITE/parser" "$SITE/queries"

# Parsers: every grammar nix built
for so in /nix/store/*-vimplugin-treesitter-grammar-*/parser/*.so; do
  [ -f "$so" ] || continue
  lang=$(basename "$so" .so)
  ln -sf "$so" "$SITE/parser/$lang.so"
done

# Queries: from nix-provided vimPlugins.nvim-treesitter
TS_NIX=$(ls -d /nix/store/*-vimplugin-nvim-treesitter-2* 2>/dev/null | head -1)
if [ -n "$TS_NIX" ] && [ -d "$TS_NIX/queries" ]; then
  for q in "$TS_NIX"/queries/*; do
    [ -d "$q" ] || continue
    lang=$(basename "$q")
    ln -sf "$q" "$SITE/queries/$lang"
  done
fi

ls "$SITE/parser/" | wc -l
ls "$SITE/queries/" | wc -l
EOF
```

This survives until the next container cutover (container state is ephemeral; only
the Dockerfile-baked version persists across redeploys).

---

## Elixir LSP (elixir-ls)

Separately from treesitter highlighting, `elixir-ls` is wired for code intelligence:

- **Installed by** `containers/apps/flake.nix:53` —
  `beam.packages.erlang_27.elixir-ls`. On PATH at
  `/nix/var/nix/profiles/default/bin/elixir-ls`.
- **Enabled by** a line appended to
  `~/.config/nvim/lua/configs/lspconfig.lua`:
  ```lua
  vim.lsp.enable("elixirls")
  ```
  This works because NvChad's `defaults()` configures `vim.lsp.config("*", ...)`
  globally, and `lspconfig` auto-registers the `elixirls.lua` default config
  when required.

### First project open is slow

elixir-ls compiles project deps on first attach (~10s with default dialyzer-on,
much longer if dialyzer needs to build its PLT). To disable dialyzer for speed:

```lua
-- In ~/.config/nvim/lua/configs/lspconfig.lua, before the enable line:
vim.lsp.config("elixirls", {
  settings = { elixirLS = { dialyzerEnabled = false } },
})
```

---

## Related files

| File | Role |
|---|---|
| `containers/apps/Dockerfile` | Contains the symlink `RUN` block that fixes highlighting |
| `containers/apps/home.nix:31` | Declares `nvim-treesitter.withAllGrammars` (parser source) |
| `containers/apps/flake.nix:53` | Installs `elixir-ls` (LSP source) |
| `~/.config/nvim/lazy-lock.json` (in container) | Pins `nvim-treesitter` to broken master commit |
| `~/.config/nvim/lua/configs/lspconfig.lua` (in container) | Gets `vim.lsp.enable("elixirls")` appended at build |
| `~/.local/share/nvim/lazy/NvChad/lua/nvchad/autocmds.lua` (in container) | FileType autocmd that calls `vim.treesitter.start` |

---

## Why not just pin lazy's nvim-treesitter to a working commit?

Considered and rejected as the primary fix:

1. **Doesn't solve parser discovery.** Even with a working plugin, lazy fetches its
   own grammars on `:TSInstall` (network at runtime, rate-limited, no compiler in
   the sandbox at runtime). We'd be rebuilding what nix already ships.
2. **Doesn't solve query bridging robustly.** A working installer would bridge
   queries, but only for languages explicitly `:TSInstall`'d. The nix symlink
   approach gets every grammar nix builds, for free.
3. **Pinning is brittle.** Someone bumps `lazy-lock.json` → broken again.

The symlink approach is decoupled from lazy/nvim-treesitter entirely. Lazy can
keep loading its broken plugin; the installer can keep crashing silently;
highlighting works because nvim's treesitter engine finds parsers and queries via
`runtimepath` without the plugin's help.

A secondary cleanup (not yet done) would be to disable NvChad's
`nvim-treesitter` spec entirely so the plugin doesn't load at all — silences the
silent errors and trims a few ms off startup. Polish task, not required for
correctness.
