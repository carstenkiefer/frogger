# Translation review notes

All catalogs except English and German are machine translations and benefit
from review by native speakers. Edit the files in [`i18n/`](../i18n) directly;
`cargo test -p frogger-i18n` checks that keys and `{placeholders}` stay intact.

## Kinyarwanda (`i18n/rw.toml`) and Runyankore (`i18n/nyn.toml`)

These two are low-resource languages; please review them completely. The
phrases below are the least certain ones.

### Runyankore
- **Blink** – no single verb was found; currently described as
  "humiriza kandi ohumuure amaisho" (close and open the eyes) in
  `auth.start`, `auth.blink`, `enroll.verify`, `auth.no_blink`.
- **Warning** – "Oburabuzi" (from *kurabura*).
- **Not secure / sensitive data** – "TIKURINZIRWE", "amakuru g'ebanga".
- **Liveness** – "ori muhangaari" (that you are alive).
- **Failed / aborted** – forms of *kuremwa* / *kwahagarikwa*.
- **Light, distance, glasses** – "omushana", "oku ori hare na kamera",
  "gilaasi ezirikwakyengyerereza munonga".
- **Device / boot** – "ekyoma", "okwatsya kompyuta".
- `enroll.rgb_needs_ack`, `enroll.calibration_failed`, best similarity in
  `cli.test.*` ("okushushana okukuru").
- English loanwords kept: screen, service, configuration, settings, file
  permissions, encryption, profile, controls.
- Confirmation word `cli.setup.yes_word` = "EGO" – is this the natural "yes"?
- Depth check (`auth.not_3d`, `cli.doctor.depth_found`): "obuzima" for depth,
  "nibusherekyere" for flat – both uncertain.

### Kinyarwanda
- French loanwords "infrarouge", "ecran", "chiffrement" (alternatives:
  native terms, e.g. "mugaragaza" for screen).
- Liveness check: "igenzura ry'uko uri muzima" (`auth.no_blink`).
- Best similarity: "igipimo cyiza cyo gusa" (`cli.test.*`).
- IR emitter controls: "uburyo bwo kugenzura".
- Authentication in `cli.pam.*`: "Kwemeza umwirondoro".
- Sensitive data: "amakuru y'ibanga"; glasses: "amadarubindi agarura urumuri
  cyane"; lid closed rephrased as "the laptop is closed".
- Confirmation word "YEGO".
- Depth check (`auth.not_3d`, `cli.doctor.depth_found`): "ubujyakuzimu" for
  depth, "irambuye" for flat.
