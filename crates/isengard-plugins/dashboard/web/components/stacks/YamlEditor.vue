<script setup lang="ts">
/*
 * YamlEditor: a thin Vue wrapper around CodeMirror 6.
 *
 * Read mode (readonly=true): renders the YAML with syntax highlighting,
 * line numbers, and no editing affordances. Same component used in edit
 * mode (readonly=false) with full editing.
 *
 * Theming pulls from /design/tokens.css via the iso-* CSS vars: no new
 * colors are introduced, so the editor inherits dashboard skinning.
 *
 * The component owns the CM EditorView instance directly (no wrapper
 * library): keeps the dependency surface flat (state + view + language
 * + commands + lang-yaml) and avoids stale Vue shim issues.
 */
import { onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { EditorState, Compartment } from '@codemirror/state'
import {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  drawSelection,
} from '@codemirror/view'
import {
  defaultHighlightStyle,
  syntaxHighlighting,
  HighlightStyle,
  indentOnInput,
  bracketMatching,
} from '@codemirror/language'
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands'
import { yaml as yamlLang } from '@codemirror/lang-yaml'
import { tags as t } from '@lezer/highlight'

interface Props {
  modelValue: string
  readonly?: boolean
  minHeight?: string
  /** When true, soft-wrap long lines instead of horizontal scroll. */
  wrap?: boolean
}

const props = withDefaults(defineProps<Props>(), {
  readonly: false,
  minHeight: '400px',
  wrap: false,
})

const emit = defineEmits<{
  (e: 'update:modelValue', v: string): void
}>()

const host = ref<HTMLDivElement | null>(null)
let view: EditorView | null = null

// Compartments let us swap configuration (readonly, line wrap) without
// rebuilding the whole state.
const readOnlyCompartment = new Compartment()
const wrapCompartment = new Compartment()

// Highlight style mapping Lezer tags onto iso-* tokens. Falls back to
// existing tokens where no dedicated slot exists (e.g., punctuation
// borrows from text-faint).
const isoHighlightStyle = HighlightStyle.define([
  { tag: t.comment, color: 'var(--iso-text-muted)', fontStyle: 'italic' },
  { tag: [t.propertyName, t.attributeName], color: 'var(--iso-accent-info)' },
  { tag: [t.string, t.special(t.string)], color: 'var(--iso-accent-success)' },
  { tag: [t.number, t.bool, t.null], color: 'var(--iso-accent-warn)' },
  { tag: t.keyword, color: 'var(--iso-accent-info)' },
  { tag: t.operator, color: 'var(--iso-text-secondary)' },
  { tag: t.punctuation, color: 'var(--iso-text-faint)' },
  { tag: t.meta, color: 'var(--iso-text-muted)' },
  { tag: t.invalid, color: 'var(--iso-accent-error)' },
])

// Editor surface theme. Backgrounds and borders are transparent: the
// host <div> wrapper supplies the panel chrome (border, radius, bg)
// matching the surrounding compose-tab cards.
const isoEditorTheme = EditorView.theme(
  {
    '&': {
      backgroundColor: 'transparent',
      color: 'var(--iso-text-primary)',
      fontSize: 'var(--iso-font-size-xs)',
      fontFamily: 'var(--iso-font-mono)',
    },
    '.cm-scroller': {
      fontFamily: 'var(--iso-font-mono)',
      lineHeight: '1.55',
    },
    '.cm-content': {
      caretColor: 'var(--iso-text-primary)',
      padding: '12px 0',
    },
    '.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--iso-text-primary)' },
    '&.cm-focused .cm-cursor': { borderLeftColor: 'var(--iso-text-primary)' },
    '&.cm-focused': { outline: 'none' },
    '.cm-gutters': {
      backgroundColor: 'transparent',
      color: 'var(--iso-text-faint)',
      border: 'none',
      borderRight: '1px solid var(--iso-border-subtle)',
    },
    '.cm-activeLine': {
      backgroundColor: 'var(--iso-bg-row-hover)',
    },
    '.cm-activeLineGutter': {
      backgroundColor: 'var(--iso-bg-row-hover)',
      color: 'var(--iso-text-secondary)',
    },
    '.cm-selectionBackground, ::selection': {
      backgroundColor: 'var(--iso-accent-info-soft)',
    },
    '&.cm-focused .cm-selectionBackground': {
      backgroundColor: 'var(--iso-accent-info-soft)',
    },
    '.cm-matchingBracket, .cm-nonmatchingBracket': {
      backgroundColor: 'transparent',
      outline: '1px solid var(--iso-border-strong)',
    },
    '.cm-lineNumbers .cm-gutterElement': {
      padding: '0 8px 0 6px',
      minWidth: '2.5em',
    },
  },
  { dark: true },
)

function buildState(doc: string): EditorState {
  return EditorState.create({
    doc,
    extensions: [
      lineNumbers(),
      highlightActiveLine(),
      highlightActiveLineGutter(),
      drawSelection(),
      history(),
      indentOnInput(),
      bracketMatching(),
      yamlLang(),
      syntaxHighlighting(isoHighlightStyle),
      // Fallback for any tag we didn't map: keeps things readable.
      syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
      isoEditorTheme,
      keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
      readOnlyCompartment.of(EditorState.readOnly.of(props.readonly)),
      wrapCompartment.of(props.wrap ? EditorView.lineWrapping : []),
      EditorView.updateListener.of((u) => {
        if (u.docChanged) {
          const v = u.state.doc.toString()
          if (v !== props.modelValue) emit('update:modelValue', v)
        }
      }),
    ],
  })
}

onMounted(() => {
  if (!host.value) return
  view = new EditorView({
    state: buildState(props.modelValue ?? ''),
    parent: host.value,
  })
})

onBeforeUnmount(() => {
  view?.destroy()
  view = null
})

// Keep the editor in sync when the parent updates modelValue
// (e.g., refresh, reload-from-conflict, cancelEdit).
watch(
  () => props.modelValue,
  (next) => {
    if (!view) return
    const current = view.state.doc.toString()
    if (next === current) return
    view.dispatch({
      changes: { from: 0, to: current.length, insert: next ?? '' },
    })
  },
)

watch(
  () => props.readonly,
  (ro) => {
    if (!view) return
    view.dispatch({
      effects: readOnlyCompartment.reconfigure(EditorState.readOnly.of(ro)),
    })
  },
)

watch(
  () => props.wrap,
  (w) => {
    if (!view) return
    view.dispatch({
      effects: wrapCompartment.reconfigure(w ? EditorView.lineWrapping : []),
    })
  },
)

defineExpose({
  focus: () => view?.focus(),
})
</script>

<template>
  <div
    ref="host"
    class="yaml-editor rounded-iso-lg border border-iso-border-subtle bg-iso-bg-elevated overflow-hidden"
    :class="readonly ? 'is-readonly' : 'is-editing'"
    :style="{ minHeight }"
  />
</template>

<style scoped>
.yaml-editor :deep(.cm-editor) {
  min-height: v-bind(minHeight);
  height: 100%;
}
.yaml-editor.is-editing:focus-within {
  border-color: var(--iso-border-strong);
}
.yaml-editor :deep(.cm-scroller) {
  overflow: auto;
}
</style>
