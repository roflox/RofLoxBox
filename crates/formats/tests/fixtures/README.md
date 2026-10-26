# Test fixtures

These files are copied unmodified from the `data/` directory of
[rekordcrate](https://github.com/Holzhaus/rekordcrate) 0.3.0, which is
licensed under the Mozilla Public License 2.0. They are real exports
written by rekordbox and are used only as test input.

- `complete_export/demo_tracks/` - USB export of the rekordbox demo tracks
- `complete_export/empty/` - USB export with no tracks
- `pdb/num_rows/` - export.pdb exercising pages with many rows
- `settings/` - `*SETTING.DAT` files, one directory per changed option
  (directory name = option and value), for mapping settings fields

Prefer adding fixtures you generated yourself (export from rekordbox to an
empty USB stick) over copying more third-party data.
