# WHATWG Encoding Standard indexes

The index files in this directory are copied unchanged from the Encoding Standard's repository, <https://github.com/whatwg/encoding>, and are read by `deco-encoding` at run time.

Copyright © WHATWG (Apple, Google, Mozilla, Microsoft). The files are licensed under the Creative Commons Attribution 4.0 International License; to the extent portions of them are incorporated into source code, those portions are licensed under the BSD 3-Clause License instead. The full text is in [LICENSE](LICENSE).

| File | Identifier (from the file) | Date | SHA-256 of the file |
| --- | --- | --- | --- |
| `index-jis0208.txt` | `cbaa91f3deb7d0841faf5c33041fc15a285da0e87e64ab802c4bf04b7c4da861` | 2024-09-18 | `341dcde7e8b984e9c7bbf5ed75c8da7c6087d47083a1a2b3ed558bfd5bef9468` |
| `index-jis0212.txt` | `83bf90dd1c591a4355730d8c4567efc499d74da7490531019ef22a879991cfb7` | 2024-09-18 | `6ac698a473822b282ff4f4d4f17873b970612ef9bd232e03941854c09b67c7ee` |
| `index-windows-1252.txt` | `e56d49d9176e9a412283cf29ac9bd613f5620462f2a080a84eceaf974cfa18b7` | 2024-09-18 | `2c5fa192d566f92b8b9e6e116255b1f94016b4ece5b9dacb3b98be70b8c60795` |
| `LICENSE` | | | `85dc6f5ccb57a6fe8c33d158f9fc8fc7ee5655a5d3db2cdd131c6a3d0f48a864` |

To update, replace the files with newer copies from the same repository and update this table. The crate's tests check the entry counts and known code points of each index.
