// Tiny client-side cron walker for the PolicyEditor's "Next 3 firings"
// preview. Phase 9d.
//
// Standard 5-field syntax: minute hour day-of-month month day-of-week.
// Supports star, step (slash N), comma lists, and from-to ranges per
// field. Any other grammar (named macros, day-of-week names, quartz
// extensions) is rejected; the UI shows "(invalid expression)" and the
// operator can rely on the server-side validator (which uses the full
// croner crate) to catch what the client missed.
//
// The walker steps minute by minute up to a 7-day horizon. That's coarse
// but sufficient for the operator-friendly cron patterns we expect
// (Sunday 02:00, every 4 hours, etc). For sparser patterns it returns
// fewer than 3 firings and the UI renders only what it found.
//
// tz resolves via Intl.DateTimeFormat's timeZone option so the preview
// reflects the operator's chosen IANA zone. Unknown zones throw; we catch
// and fall through to UTC.

interface ParsedField {
  /** `true` means "match every value"; ignore the values array. */
  any: boolean
  /** Allowed values within the field's range. Sorted, deduplicated. */
  values: number[]
}

interface ParsedCron {
  minute: ParsedField
  hour: ParsedField
  dom: ParsedField
  month: ParsedField
  dow: ParsedField
}

const RANGES: Record<keyof ParsedCron, [number, number]> = {
  minute: [0, 59],
  hour: [0, 23],
  dom: [1, 31],
  month: [1, 12],
  dow: [0, 6],
}

function parseField(spec: string, [lo, hi]: [number, number]): ParsedField | null {
  if (spec === '*') return { any: true, values: [] }
  const set = new Set<number>()
  for (const part of spec.split(',')) {
    if (part === '') return null
    let stride = 1
    let body = part
    const slash = part.indexOf('/')
    if (slash !== -1) {
      stride = Number.parseInt(part.slice(slash + 1), 10)
      if (!Number.isFinite(stride) || stride < 1) return null
      body = part.slice(0, slash)
    }
    let from = lo
    let to = hi
    if (body !== '*' && body !== '') {
      const dash = body.indexOf('-')
      if (dash !== -1) {
        from = Number.parseInt(body.slice(0, dash), 10)
        to = Number.parseInt(body.slice(dash + 1), 10)
      } else {
        from = Number.parseInt(body, 10)
        to = stride === 1 ? from : hi
      }
      if (!Number.isFinite(from) || !Number.isFinite(to)) return null
      if (from < lo || to > hi || from > to) return null
    }
    for (let v = from; v <= to; v += stride) set.add(v)
  }
  if (set.size === 0) return null
  return { any: false, values: [...set].sort((a, b) => a - b) }
}

export function parseCronExpression(expr: string): ParsedCron | null {
  const parts = expr.trim().split(/\s+/)
  if (parts.length !== 5) return null
  const minute = parseField(parts[0], RANGES.minute)
  const hour = parseField(parts[1], RANGES.hour)
  const dom = parseField(parts[2], RANGES.dom)
  const month = parseField(parts[3], RANGES.month)
  const dow = parseField(parts[4], RANGES.dow)
  if (!minute || !hour || !dom || !month || !dow) return null
  return { minute, hour, dom, month, dow }
}

function fieldMatches(f: ParsedField, value: number): boolean {
  return f.any || f.values.includes(value)
}

interface ParsedTime {
  year: number
  month: number
  day: number
  hour: number
  minute: number
  weekday: number
}

/**
 * Decompose a `Date` into its calendar parts in the chosen IANA timezone.
 * Falls back to UTC on unknown timezones.
 */
function decompose(date: Date, tz: string): ParsedTime {
  let opts: Intl.DateTimeFormatOptions = {
    timeZone: tz,
    year: 'numeric',
    month: 'numeric',
    day: 'numeric',
    hour: 'numeric',
    minute: 'numeric',
    weekday: 'short',
    hour12: false,
  }
  let parts: Intl.DateTimeFormatPart[]
  try {
    parts = new Intl.DateTimeFormat('en-US', opts).formatToParts(date)
  } catch {
    opts = { ...opts, timeZone: 'UTC' }
    parts = new Intl.DateTimeFormat('en-US', opts).formatToParts(date)
  }
  const get = (t: string) => parts.find(p => p.type === t)?.value ?? '0'
  const wmap: Record<string, number> = {
    Sun: 0, Mon: 1, Tue: 2, Wed: 3, Thu: 4, Fri: 5, Sat: 6,
  }
  return {
    year: Number.parseInt(get('year'), 10),
    month: Number.parseInt(get('month'), 10),
    day: Number.parseInt(get('day'), 10),
    hour: Number.parseInt(get('hour'), 10) % 24,
    minute: Number.parseInt(get('minute'), 10),
    weekday: wmap[get('weekday')] ?? 0,
  }
}

/**
 * Find the next `count` firings of `expr` in `tz` after `from`, returning
 * Date objects (UTC instants). Bounded to a 7-day lookahead to keep the
 * walker cheap; sparse patterns may return fewer than `count` results.
 *
 * Returns an empty array on parse failure so callers can render a "(invalid
 * expression)" hint without distinguishing from "no firings in 7 days".
 */
export function nextFirings(
  expr: string,
  tz: string,
  from: Date,
  count: number,
): Date[] {
  const parsed = parseCronExpression(expr)
  if (!parsed) return []

  const out: Date[] = []
  const horizon = 7 * 24 * 60 // minutes
  const cursor = new Date(from.getTime())
  // Step to the next minute boundary so we never re-emit the current minute.
  cursor.setUTCSeconds(0, 0)
  cursor.setUTCMinutes(cursor.getUTCMinutes() + 1)

  for (let i = 0; i < horizon && out.length < count; i++) {
    const t = decompose(cursor, tz)
    if (
      fieldMatches(parsed.minute, t.minute)
      && fieldMatches(parsed.hour, t.hour)
      && fieldMatches(parsed.month, t.month)
      && (
        // POSIX cron OR semantics: when both dom and dow are constrained,
        // either matching is enough. When only one is constrained, that
        // one must match.
        (parsed.dom.any && parsed.dow.any)
        || (!parsed.dom.any && parsed.dow.any && fieldMatches(parsed.dom, t.day))
        || (parsed.dom.any && !parsed.dow.any && fieldMatches(parsed.dow, t.weekday))
        || (!parsed.dom.any && !parsed.dow.any
          && (fieldMatches(parsed.dom, t.day) || fieldMatches(parsed.dow, t.weekday)))
      )
    ) {
      out.push(new Date(cursor.getTime()))
    }
    cursor.setUTCMinutes(cursor.getUTCMinutes() + 1)
  }
  return out
}

/**
 * Format a Date for display next to the cron input. Renders the chosen
 * timezone explicitly so the operator sees the wall-clock time they're
 * actually scheduling.
 */
export function formatFiring(date: Date, tz: string): string {
  const opts: Intl.DateTimeFormatOptions = {
    timeZone: tz,
    weekday: 'short',
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  }
  try {
    return new Intl.DateTimeFormat('en-US', opts).format(date)
  } catch {
    return new Intl.DateTimeFormat('en-US', { ...opts, timeZone: 'UTC' }).format(date)
  }
}
