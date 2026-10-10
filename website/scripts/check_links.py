"""Check rendered local links, fragments and assets without network access."""
import os
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urljoin, urlsplit

ROOT = Path(__file__).resolve().parents[1] / 'dist'
# dist/ is served under the deploy base (SITE_BASE=/rness on GitHub Pages).
BASE = '/' + os.environ.get('SITE_BASE', '').strip('/') + '/' if os.environ.get('SITE_BASE', '').strip('/') else '/'


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__()
        self.ids, self.links = set(), []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if 'id' in attrs:
            self.ids.add(attrs['id'])
        for name in ('href', 'src'):
            if name in attrs:
                self.links.append(attrs[name])


pages = {p: Page(p.read_text()) for p in ROOT.rglob('*.html')}
errors = []
for file, page in pages.items():
    url = BASE + file.relative_to(ROOT).as_posix().removesuffix('index.html')
    for link in page.links:
        target = urlsplit(urljoin(url, link))
        if target.scheme or target.netloc:
            continue
        if not target.path.startswith(BASE):
            errors.append(f'{url}: outside base {BASE}: {link}')
            continue
        dest = ROOT / unquote(target.path)[len(BASE):]
        if dest.is_dir():
            dest /= 'index.html'
        if not dest.exists():
            errors.append(f'{url}: missing {link}')
        elif target.fragment and dest in pages and unquote(target.fragment) not in pages[dest].ids:
            errors.append(f'{url}: missing anchor {link}')
for error in errors:
    print(error)
print(f'Checked {len(pages)} HTML pages; {len(errors)} broken links/anchors/assets.')
raise SystemExit(bool(errors))
