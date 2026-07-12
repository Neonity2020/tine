import { canon } from "./vendor/compare.mjs";
import type { Projection } from "./mldoc-client";

interface Comparison {
  matches: boolean;
  shifts: number;
}

type JsonObject = Record<string, unknown>;

/**
 * Recognize the exact issue #82 artifact caused by mldoc's leaked
 * `end_string("``")` rolling window. This deliberately does not treat arbitrary
 * code-span differences as equivalent: the only accepted delta moves one
 * literal backtick from the start of a Code node to the preceding Plain node.
 */
export function isMldocBacktickStateArtifact(lsdoc: Projection, mldoc: Projection): boolean {
  const left = canon({ blocks: lsdoc.blocks, refs: lsdoc.refs });
  const right = canon({ blocks: mldoc.blocks, refs: mldoc.refs });
  const result = compare(left, right);
  return result.matches && result.shifts > 0;
}

export function shouldQuarantineMldocBacktickStateArtifact(
  freshRangeParsesAgree: boolean,
  lsdoc: Projection,
  mldoc: Projection,
): boolean {
  return freshRangeParsesAgree && isMldocBacktickStateArtifact(lsdoc, mldoc);
}

function compare(left: unknown, right: unknown): Comparison {
  if (Object.is(left, right)) return { matches: true, shifts: 0 };
  if (Array.isArray(left) || Array.isArray(right)) {
    if (!Array.isArray(left) || !Array.isArray(right) || left.length !== right.length) {
      return { matches: false, shifts: 0 };
    }
    let shifts = 0;
    for (let i = 0; i < left.length; i++) {
      if (i + 1 < left.length && isBacktickOwnershipShift(left[i], left[i + 1], right[i], right[i + 1])) {
        shifts++;
        i++;
        continue;
      }
      const item = compare(left[i], right[i]);
      if (!item.matches) return item;
      shifts += item.shifts;
    }
    return { matches: true, shifts };
  }
  if (isObject(left) || isObject(right)) {
    if (!isObject(left) || !isObject(right)) return { matches: false, shifts: 0 };
    const leftKeys = Object.keys(left);
    const rightKeys = Object.keys(right);
    if (leftKeys.length !== rightKeys.length || leftKeys.some((key) => !Object.hasOwn(right, key))) {
      return { matches: false, shifts: 0 };
    }
    let shifts = 0;
    for (const key of leftKeys) {
      const field = compare(left[key], right[key]);
      if (!field.matches) return field;
      shifts += field.shifts;
    }
    return { matches: true, shifts };
  }
  return { matches: false, shifts: 0 };
}

function isBacktickOwnershipShift(
  lsdocPlain: unknown,
  lsdocCode: unknown,
  mldocPlain: unknown,
  mldocCode: unknown,
): boolean {
  if (![lsdocPlain, lsdocCode, mldocPlain, mldocCode].every(isTextInline)) return false;
  return lsdocPlain.k === "plain"
    && lsdocCode.k === "code"
    && mldocPlain.k === "plain"
    && mldocCode.k === "code"
    && mldocPlain.text === `${lsdocPlain.text}\``
    && lsdocCode.text === `\`${mldocCode.text}`;
}

function isTextInline(value: unknown): value is { k: string; text: string } {
  return isObject(value)
    && Object.keys(value).length === 2
    && typeof value.k === "string"
    && typeof value.text === "string";
}

function isObject(value: unknown): value is JsonObject {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
