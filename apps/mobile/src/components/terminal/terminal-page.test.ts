import { MAX_FONT, MIN_FONT, terminalPage } from './terminal-page';

/**
 * The page is a string, so nothing else in the build ever parses it: a typo in
 * the bridge script is invisible to `tsc` and to lint, and only shows up as a
 * terminal that renders nothing on a device.
 */
function bridgeScript(html: string): string {
  const open = html.lastIndexOf('<script>');
  const close = html.lastIndexOf('</script>');
  expect(open).toBeGreaterThan(-1);
  expect(close).toBeGreaterThan(open);
  return html.slice(open + '<script>'.length, close);
}

describe('terminalPage', () => {
  it('emits a bridge script that parses', () => {
    // `new Function` compiles the body without running it — the xterm bundle and
    // the DOM it wants are not here, and syntax is the whole question.
    expect(() => new Function(bridgeScript(terminalPage('#000', '#fff', 'mirror')))).not.toThrow();
    expect(() => new Function(bridgeScript(terminalPage('#000', '#fff', 'reflow')))).not.toThrow();
  });

  it('bakes in the requested mode', () => {
    expect(bridgeScript(terminalPage('#000', '#fff', 'mirror'))).toContain('var mode = "mirror"');
    expect(bridgeScript(terminalPage('#000', '#fff', 'reflow'))).toContain('var mode = "reflow"');
  });

  it('reaches the grid fit only on the reflow branch', () => {
    // Counting `post({type:'resize'})` call sites is deliberately NOT the check:
    // mirror legitimately posts one too, to hand the host's size back on the way
    // out of reflow. Which branch runs when is a behavioural question — see
    // terminal-page.behavior.test.ts, which executes this script.
    expect(bridgeScript(terminalPage('#000', '#fff', 'mirror'))).toMatch(
      /if \(mode === 'reflow'\) fitGrid\(\);/
    );
  });

  it('clamps the font to the readable band', () => {
    const script = bridgeScript(terminalPage('#000', '#fff', 'mirror'));
    expect(script).toContain(`var MIN_FONT = ${MIN_FONT};`);
    expect(script).toContain(`var MAX_FONT = ${MAX_FONT};`);
    expect(MIN_FONT).toBeLessThan(MAX_FONT);
  });

  it('escapes the theme colours it interpolates', () => {
    // Colours reach here from the theme, but they land inside a script; a value
    // carrying a quote would end the string and run whatever followed.
    const script = bridgeScript(terminalPage('"+alert(1)+"', '#fff', 'mirror'));
    expect(script).toContain(String.raw`"\"+alert(1)+\""`);
  });
});
