import { render, screen } from '@testing-library/react-native';

import { UserBubble } from '@/components/chat/entries';
import { dataUri } from '@/components/chat/image-lightbox';
import type { ChatImage } from '@/native/thread';

const IMAGE: ChatImage = { media_type: 'image/png', data: 'aGVsbG8=' };

describe('UserBubble attachments', () => {
  it('renders an attached image rather than describing it', () => {
    render(<UserBubble text="Test thử ảnh xem nào" images={[IMAGE]} />);

    // The regression this pins: the phone used to print "1 image attached"
    // while the desktop showed the picture, even though the snapshot carries
    // the bytes. Asserting the image alone would still pass with the caption
    // left in place, so the absence of that text is half the assertion.
    expect(screen.getByLabelText('View attached image')).toBeTruthy();
    expect(screen.queryByText(/image attached/)).toBeNull();
    expect(screen.getByText('Test thử ảnh xem nào')).toBeTruthy();
  });

  it('gives each attachment its own thumbnail', () => {
    render(<UserBubble text="two" images={[IMAGE, IMAGE]} />);

    expect(screen.getAllByLabelText('View attached image')).toHaveLength(2);
  });

  it('builds a data URI the image loader can resolve', () => {
    // Asserted against the helper rather than the rendered props: expo-image
    // rewrites `source` into its own shape, so reaching into the tree would test
    // the library's normalisation instead of the URI this code produces.
    expect(dataUri(IMAGE)).toBe('data:image/png;base64,aGVsbG8=');
  });

  it('renders no bubble for an image-only prompt', () => {
    const { toJSON } = render(<UserBubble text="" images={[IMAGE]} />);

    // An empty rounded box under the thumbnail reads as a rendering fault, so
    // the bubble is skipped entirely when the prompt carried no words.
    expect(JSON.stringify(toJSON())).not.toContain('borderTopRightRadius');
  });

  it('leaves a text-only message untouched', () => {
    render(<UserBubble text="no attachments" images={[]} />);

    expect(screen.getByText('no attachments')).toBeTruthy();
    expect(screen.queryByLabelText('View attached image')).toBeNull();
  });
});
