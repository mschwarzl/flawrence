# Flawrence website

A standalone Zola 0.22.1 site; Python 3 standard library only for importing the launcher branding, the canonical findings and the reference pages from `docs/`. No JavaScript or theme dependencies.

From the repository root:

```sh
python3 website/prepare.py
cd website
zola serve
```

For production, repeat preparation after changes to the launcher, `FINDINGS.md` or `docs/`, then run `zola build --base-url https://YOUR-DOMAIN`. Publish `website/public/` on a static host. The default URL assumes GitHub Pages under `/flawrence`; replace it when using a custom domain. The existing root Dockerfile remains exclusively for the application.

Run details are optional. To add them for a repository on the Findings page, copy `runs/TEMPLATE.md` to `runs/<repository>.md` (`a-sit-plus/vck` becomes `vck.md`) and write plain Markdown: model, cost, prompt and notes. Delete what you do not have. Repositories without a file, or with only comments in it, show no run details. Only add information cleared for publication, never credentials or private reports.

The screenshot on the browser launcher page uses the existing README attachment URL and is cropped visually; click to see the full image. To self-host it, download it into `static/` and change `extra.screenshot` in `config.toml`.

The original launcher harp is attributed in the footer. The site imports the original CSS branding rules rather than duplicating the palette in a separate theme.
