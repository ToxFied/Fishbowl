# Fishbowl brand

Fishbowl is a SQL database you can see inside. The mark shows that directly: a glass bowl with rows of data as the water, and a goldfish (the query) swimming through them.

## Name and tagline

- Write the name as "Fishbowl" in text. The wordmark is lowercase: `fishbowl`.
- Tagline: "A SQL database you can see inside."

## Color

Three colors carry meaning, both in the brand and in the UI.

| Role | Value | Use |
| --- | --- | --- |
| Glass | `#FFFFFF` | Structure: text, outlines, plan trees |
| Water | `#000000` | Background. Always true black |
| Goldfish | `#FF7A1A` | The live query: active rows, the chosen plan, the current step |
| Data at rest | `#FFFFFF` at 34% | Rows and nodes the query hasn't touched |

Orange is reserved for whatever the query is doing right now. If everything is orange, nothing is.

## Type

- **Geist SemiBold** for the wordmark and headings.
- **Geist** for UI text.
- **Geist Mono** for SQL, values, and plan output.

All three are free under the SIL Open Font License (`npm i geist`).

## Assets

| File | Use |
| --- | --- |
| `icon.svg`, `icon-1024.png` | App icon, detailed |
| `favicon.svg`, `favicon-32.png` | Browser tab and small sizes. Heavier strokes, no rows |
| `apple-touch-icon.png` | iOS home screen, 180px |
| `mark.svg` | The mark without its tile, for dark backgrounds |
| `wordmark.svg` | `fishbowl` in white |
| `lockup.svg` | Mark and wordmark side by side, for dark backgrounds |
| `lockup-light.svg` | The same in black ink, for light backgrounds such as GitHub's light theme |
| `social-card.png` | GitHub social preview, 1280x640 |
| `src/mark.py` | Generates the mark. Run `python3 src/mark.py detail\|favicon\|mono` |

The repo-root `fishbowl-mark.svg` is the favicon version, for the README and the T3 Code project icon.

## Don't

- Put the white mark on a light background. Use `lockup-light.svg` there instead.
- Recolor the fish or add a second accent color.
- Use the detailed icon below 48px. Use the favicon version instead.
