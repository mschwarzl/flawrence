#!/usr/bin/env python3
"""Copy branding and render findings and the docs/ references into Zola content."""
from pathlib import Path
import re
import tomllib
import shutil

site = Path(__file__).resolve().parent
root = site.parent
src = root / 'bin/flawrence-runner/src'
shutil.copyfile(src / 'harp.svg', site / 'static/harp.svg')
html = (src / 'launcher.html').read_text()
css = re.search(r'<style>(.*?)</style>', html, re.S).group(1)
# Reuse only the launcher rules relevant to branding, not its form UI.
selectors = [':root', '::selection', 'h1', '.harp', '.harp::before, .harp::after, .harp-mark', '.harp::before', '.harp::after', '.harp-mark']
parts = []
for selector in selectors:
    match = re.search(r'(?:^|\n)\s*' + re.escape(selector) + r'\s*\{[^}]*\}', css)
    if not match:
        raise ValueError(f'Launcher selector changed: {selector}')
    parts.append(match.group().strip().replace('/launcher/harp.svg', 'harp.svg'))
(site / 'static/launcher-brand.css').write_text('\n'.join(parts) + '\n')
source = (root / 'FINDINGS.md').read_text()
source = re.sub(r'^# Findings\s*\n', '', source, count=1)
source = re.sub(r'\]\((disclosures[^)]*)\)', lambda m: '](https://github.com/mschwarzl/flawrence/' + ('blob/main/' if m[1].endswith('.md') else 'tree/main/') + m[1] + ')', source)
sections = re.split(r'(?m)^## (.+)\n', source)
output = ['+++\ntitle = "Findings"\n+++\n', sections[0]]
def run_details(name):
    """Return a collapsible block from runs/<repo>.md, or nothing when there is no real content."""
    path = site / 'runs' / (name.rsplit('/', 1)[-1] + '.md')
    if not path.is_file():
        return ''
    text = path.read_text().strip()
    # A copied but unfilled template holds only comments, so it adds nothing.
    if not re.sub(r'<!--.*?-->', '', text, flags=re.S).strip():
        return ''
    # Headings would show up in "On this page", so they become bold lines.
    lines, fenced = [], False
    for line in text.split('\n'):
        fenced ^= bool(re.match(r'(?:>\s?)*\s*(```|~~~)', line))
        lines.append(line if fenced else re.sub(r'^((?:>\s?)*)#{1,6}\s+(.+?)\s*#*$', r'\1**\2**', line))
    text = '\n'.join(lines)
    # Zola would read braces as shortcodes, for example the {{placeholders}} in prompts.
    text = text.replace('{{', '{{ "{{" }}').replace('{%', '{{ "{%" }}')
    return '<details class="run-details">\n<summary>Run details</summary>\n\n' + text + '\n\n</details>\n'

for name, body in zip(sections[1::2], sections[2::2]):
    output.extend([f'## {name}\n', body, run_details(name)])
(site / 'content/findings.md').write_text('\n'.join(output))

# Reference pages come straight from docs/. Order and weight here decide the guide sidebar.
REFERENCES = [('web-launcher', 20), ('configuration', 30), ('agent-memory', 40), ('context-checkpoints', 50)]
for slug, weight in REFERENCES:
    text = (root / 'docs' / (slug + '.md')).read_text()
    title = re.match(r'# (.+)\n', text)[1]
    text = text[text.index("\n") + 1:]
    # Links between docs become Zola internal links, so a rename fails the build.
    text = re.sub(r'\]\(([a-z-]+\.md(?:#[^)]*)?)\)', lambda m: '](@/guide/' + m[1] + ')', text)
    front = f'+++\ntitle = "{title}"\nweight = {weight}\n\n[extra]\nsource = "docs/{slug}.md"\n+++\n'
    if slug == 'web-launcher':
        # The launcher page carries the screenshot; it links to the full-size image.
        with (site / 'config.toml').open('rb') as stream:
            shot = tomllib.load(stream)['extra']['screenshot']
        text = ('\n<figure class="screenshot"><a href="' + shot + '"><img src="' + shot + '" alt="Flawrence browser launcher configuration screen" width="2388" height="8630" loading="lazy"></a>'
                '<figcaption class="muted">Select the screenshot to see the complete launcher.</figcaption></figure>\n' + text)
    (site / 'content/guide' / (slug + '.md')).write_text(front + text)
