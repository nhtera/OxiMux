import { render, screen, fireEvent } from '@testing-library/react-native';
import { Linking } from 'react-native';

import { MarkdownBody } from '@/components/chat/markdown-body';

jest.spyOn(Linking, 'openURL').mockResolvedValue(true);

beforeEach(() => {
  (Linking.openURL as jest.Mock).mockClear();
});

describe('MarkdownBody', () => {
  it('renders markdown as formatted text rather than raw source', () => {
    render(<MarkdownBody text={'## Heading two\n\nSome **bold** words.'} />);

    // The assertion that matters is the absence of syntax: before this phase the
    // transcript showed the literal `##` and `**` the model emitted. Checking
    // that the text arrived is not enough — plain-text rendering passes that too.
    expect(screen.getByText('Heading two')).toBeTruthy();
    expect(screen.queryByText(/## Heading two/)).toBeNull();
    expect(screen.queryByText(/\*\*bold\*\*/)).toBeNull();
  });

  it('renders list items', () => {
    render(<MarkdownBody text={'- first\n- second'} />);
    expect(screen.getByText('first')).toBeTruthy();
    expect(screen.getByText('second')).toBeTruthy();
  });

  it('renders a fenced code block with its contents intact', () => {
    render(<MarkdownBody text={'```bash\necho hello\n```'} />);
    // The fence body must survive tokenizing and re-assembly into Text runs —
    // highlighting splits the source into spans, which is exactly where content
    // can get dropped or reordered without anything erroring.
    expect(screen.getByText(/echo/)).toBeTruthy();
  });

  it('opens an https link when tapped', () => {
    render(<MarkdownBody text={'[docs](https://example.com/page)'} />);
    fireEvent.press(screen.getByText('docs'));
    expect(Linking.openURL).toHaveBeenCalledWith('https://example.com/page');
  });

  // Agent output is model-generated text, and a model can be steered by whatever
  // it read — a web page, a file, a tool result. So a link in a reply is
  // untrusted input, not something the user chose to navigate to. These are the
  // schemes that turn a tapped link into code execution or a spoofed document.
  it.each([
    ['javascript:', 'javascript:alert(1)'],
    ['data:', 'data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg=='],
    ['file:', 'file:///etc/passwd'],
  ])('refuses to open a %s link', (_label, href) => {
    render(<MarkdownBody text={`[tap me](${href})`} />);
    fireEvent.press(screen.getByText('tap me'));
    expect(Linking.openURL).not.toHaveBeenCalled();
  });

  it('ignores a malformed href instead of throwing', () => {
    render(<MarkdownBody text={'[bare](not-a-url)'} />);
    // A model writing a bare path is far more likely than a user wanting a
    // browser for it, so this must be a silent no-op — not a crash, and not a
    // navigation.
    expect(() => fireEvent.press(screen.getByText('bare'))).not.toThrow();
    expect(Linking.openURL).not.toHaveBeenCalled();
  });
});
