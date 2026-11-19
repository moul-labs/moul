import { describe, expect, it } from 'vitest';
import { ruleErrorsOf } from '../src/lib/rule-errors';

const refused = (data: unknown) => Object.assign(new Error('refused'), { status: 400, data });

describe('the rule a refused save names', () => {
  it('goes under that rule, without the prefix the input already says', () => {
    const err = refused({ message: 'Invalid listRule: invalid field "ownr"', rule: 'listRule' });
    expect(ruleErrorsOf(err)).toEqual({ listRule: 'invalid field "ownr"' });
  });

  it('drops the collection a schema file names too', () => {
    const err = refused({
      message: 'Invalid viewRule of the collection "tasks": failed to tokenize filter: unexpected character \'!\' at position 0',
      rule: 'viewRule',
    });
    expect(ruleErrorsOf(err)).toEqual({ viewRule: "failed to tokenize filter: unexpected character '!' at position 0" });
  });

  it('is nothing for other errors and for rules without an input', () => {
    expect(ruleErrorsOf(refused({ message: 'Moul name is required' }))).toEqual({});
    expect(ruleErrorsOf(refused({ message: 'Invalid subscribeRule: x', rule: 'subscribeRule' }))).toEqual({});
    expect(ruleErrorsOf(new Error('offline'))).toEqual({});
    expect(ruleErrorsOf(null)).toEqual({});
  });
});
