# Track B - DS#2 media link durability

## Summary

Optimistic paste/capture asset insertion still commits the markdown link immediately so the success path keeps the fast paste feel, but the background `saveAsset` promise is now tracked by the persistence close barrier and a rejected write removes the exact inserted asset markdown occurrence from the block before showing the existing error toast. `flushAll()` now drains pending asset writes alongside page writes, which lets app close wait for in-flight asset bytes and gives any rollback-dirtied page another flush pass before the close decision.

## SHA

Final commit SHA is printed by the `DONE_TRACK_B` completion line. It cannot be embedded in this committed file without changing the commit SHA.

## Touched files

- `src/components/Block.tsx`
- `src/media.ts`
- `src/persistence.ts`
- `src/store.ts`
- `src/components/Block.assetPaste.test.tsx`
- `src/persistence.test.ts`
- `subagent-tasks/overnight-datasafety/notes/track-B.md`

## Evidence

- RED before implementation: `rtk ./node_modules/.bin/vitest run --config vitest.render.config.ts src/components/Block.assetPaste.test.tsx` failed with `expected '![](../assets/20260709-231306-481-1.png)' not to contain '../assets/'`, proving the rejected asset write left a dangling link.
- GREEN after implementation: `rtk ./node_modules/.bin/vitest run --config vitest.render.config.ts src/components/Block.assetPaste.test.tsx` passed.
- Close-barrier test: `rtk ./node_modules/.bin/vitest run src/persistence.test.ts` passed.
- Regression suite: `rtk npm test` passed (`75` node test files / `703` tests and `34` render test files / `308` tests).
