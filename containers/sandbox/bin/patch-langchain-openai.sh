#!/bin/bash
# Patches @langchain/openai completions converter in-place to force assistant
# role on streaming chunks that carry tool_calls/content but no explicit role.
#
# GLM-5.x omits role on mid-stream chunks; without this, langchain's
# aggregator falls back to ChatMessageChunk, which deepagents' middleware
# rejects with "expected AIMessage or Command, got object".
#
# Idempotent: skips files already patched.
set -e

GUARD="const fallbackRole = (delta.tool_calls"
REPLACEMENT='const fallbackRole = (delta.tool_calls || delta.content || delta.reasoning_content) ? "assistant" : defaultRole; const role = delta.role ?? fallbackRole;'

for f in "$@"; do
  if [ ! -f "$f" ]; then
    echo "[patch-langchain] skip (missing): $f"
    continue
  fi
  if grep -q "$GUARD" "$f"; then
    echo "[patch-langchain] skip (already patched): $f"
    continue
  fi
  # Use perl for precise matching — sed `|` separator clashes with shell quoting.
  perl -i -pe "s|\Qconst role = delta.role \?\? defaultRole;\E|$REPLACEMENT|g" "$f"
  echo "[patch-langchain] patched: $f"
done
