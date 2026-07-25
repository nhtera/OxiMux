import { fireEvent, render, screen } from '@testing-library/react-native';

import { ProjectGroupHeader } from '@/components/sessions/project-group-header';

describe('ProjectGroupHeader', () => {
  it('reports how many sessions are hidden while it is shut', () => {
    render(<ProjectGroupHeader name="OxiMux" count={3} expanded={false} onToggle={() => {}} />);

    // The count only earns its place while collapsed: with the rows on screen it
    // is arithmetic the user can already do.
    expect(screen.getByText('3')).toBeTruthy();
  });

  it('drops the count once the rows themselves are showing', () => {
    render(<ProjectGroupHeader name="OxiMux" count={3} expanded onToggle={() => {}} />);

    expect(screen.queryByText('3')).toBeNull();
  });

  it('folds on a press anywhere across the name, not just the chevron', () => {
    const onToggle = jest.fn();
    render(<ProjectGroupHeader name="OxiMux" count={3} expanded onToggle={onToggle} />);

    fireEvent.press(screen.getByLabelText('OxiMux, 3 sessions'));

    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it('keeps composing separate from folding', () => {
    const onToggle = jest.fn();
    const onCompose = jest.fn();
    render(
      <ProjectGroupHeader
        name="OxiMux"
        count={1}
        expanded
        onToggle={onToggle}
        onCompose={onCompose}
      />
    );

    fireEvent.press(screen.getByLabelText('New session in OxiMux'));

    // Tapping compose must not also collapse the project the new session lands in.
    expect(onCompose).toHaveBeenCalledTimes(1);
    expect(onToggle).not.toHaveBeenCalled();
  });

  it('offers no compose affordance without a project to start one in', () => {
    render(<ProjectGroupHeader name="Other" count={2} expanded onToggle={() => {}} />);

    // The bucket of sessions matching no project has no path, so there is nowhere
    // for a new session to be created.
    expect(screen.queryByLabelText('New session in Other')).toBeNull();
  });
});
