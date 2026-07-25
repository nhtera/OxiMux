import { fireEvent, render, screen } from '@testing-library/react-native';

import { Composer } from '@/components/chat/composer';

/**
 * The `draft` prefill — how an attached PR/issue reaches the composer.
 *
 * Scoped to that one prop. The composer's send/steer behaviour is exercised
 * through the session screen; what is easy to get wrong here is the seeding
 * rule, which has to apply a new draft without ever overwriting what the user
 * has typed since.
 */
function setup(draft?: string) {
  const onSend = jest.fn().mockResolvedValue(true);
  const view = render(
    <Composer
      turnActive={false}
      draft={draft}
      onSend={onSend}
      onSteer={jest.fn().mockResolvedValue(true)}
      onCancel={jest.fn().mockResolvedValue(undefined)}
      onError={jest.fn()}
    />
  );
  return { onSend, view };
}

function input() {
  return screen.getByPlaceholderText(/send a prompt/i);
}

it('seeds the input from a draft', () => {
  setup('Fix the parser (#42)\nhttps://example.test/pull/42');

  expect(input().props.value).toContain('Fix the parser (#42)');
});

it('leaves the input empty when there is no draft', () => {
  setup(undefined);

  expect(input().props.value).toBe('');
});

it('does not overwrite what the user typed after the draft landed', () => {
  const { view } = setup('attached context');

  fireEvent.changeText(input(), 'attached context — and please also add tests');
  // A re-render with the SAME draft must not reassert it: a prefill that fought
  // the user's typing would make the field unusable after an attach.
  view.rerender(
    <Composer
      turnActive={false}
      draft="attached context"
      onSend={jest.fn().mockResolvedValue(true)}
      onSteer={jest.fn().mockResolvedValue(true)}
      onCancel={jest.fn().mockResolvedValue(undefined)}
      onError={jest.fn()}
    />
  );

  expect(input().props.value).toBe('attached context — and please also add tests');
});

it('applies a genuinely new draft', () => {
  const { view } = setup('first attachment');

  fireEvent.changeText(input(), 'edited by hand');
  // Attaching a *different* item should replace the field — that is the user
  // asking for it, not the prefill reasserting itself.
  view.rerender(
    <Composer
      turnActive={false}
      draft="second attachment"
      onSend={jest.fn().mockResolvedValue(true)}
      onSteer={jest.fn().mockResolvedValue(true)}
      onCancel={jest.fn().mockResolvedValue(undefined)}
      onError={jest.fn()}
    />
  );

  expect(input().props.value).toBe('second attachment');
});
