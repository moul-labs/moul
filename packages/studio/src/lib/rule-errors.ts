import type { MoulRules } from '../components/collections/rules-editor';

/** A message per rule, shown under its input in the rules editor. */
export type RuleErrors = Partial<Record<keyof MoulRules, string>>;

const EDITED: ReadonlyArray<keyof MoulRules> = ['listRule', 'viewRule', 'createRule', 'updateRule', 'deleteRule'];

/**
 * The rule a failed save names, with the engine's message: a save whose rule does not compile
 * answers 400 with `rule` (`"listRule"`) and `message` (`Invalid listRule: …`). Empty for any other
 * error, and for a rule the editor has no input for.
 */
export function ruleErrorsOf(err: unknown): RuleErrors {
  const data = (err as { data?: { rule?: unknown; message?: unknown } } | null)?.data;
  const key = EDITED.find((k) => k === data?.rule);
  if (!key || typeof data?.message !== 'string') return {};
  // The input already says which rule (and which collection) it is.
  return { [key]: data.message.replace(/^Invalid \w+(?: of the collection "[^"]*")?: /, '') };
}
