import { fireEvent, render, screen } from '@testing-library/react-native';

import { SubagentLogPanel } from '@/components/chat/subagent-log-panel';

describe('SubagentLogPanel', () => {
  it('renders nothing when there is no subagent output', () => {
    // Most tool calls have none, so this is the common path: the panel must not
    // leave an empty toggle on every card in the transcript.
    const { toJSON } = render(<SubagentLogPanel lines={[]} />);
    expect(toJSON()).toBeNull();
  });

  it('stays collapsed until tapped', () => {
    render(<SubagentLogPanel lines={['first line', 'second line']} />);
    expect(screen.getByText(/Subagent activity \(2\)/)).toBeTruthy();
    expect(screen.queryByText('first line')).toBeNull();
  });

  it('reveals the log lines when expanded', () => {
    render(<SubagentLogPanel lines={['first line', 'second line']} />);
    fireEvent.press(screen.getByText(/Subagent activity/));
    expect(screen.getByText('first line')).toBeTruthy();
    expect(screen.getByText('second line')).toBeTruthy();
  });

  it('renders log lines literally rather than as markdown', () => {
    // A log line beginning with `#` or containing `_` is a comment or an
    // identifier, not a heading or emphasis. Running these through the markdown
    // renderer would silently rewrite the agent's own output.
    render(<SubagentLogPanel lines={['# not a heading', 'calls some_snake_case()']} />);
    fireEvent.press(screen.getByText(/Subagent activity/));
    expect(screen.getByText('# not a heading')).toBeTruthy();
    expect(screen.getByText('calls some_snake_case()')).toBeTruthy();
  });
});
