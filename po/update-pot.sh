#!/bin/sh
# Regenerates po/qlinuxdevicemanager.pot from every tr("...") call in the Rust sources.
# Strings built at run time (tr(variable)) are not extracted; keep their literals in a
# tr("...") call at the point they are defined.
set -eu
cd "$(dirname "$0")/.."
find crates -name '*.rs' -not -path '*/tests/*' | sort | xargs xgettext \
    --language=Rust --keyword=tr --from-code=UTF-8 --add-comments=TRANSLATORS \
    --package-name=QLinuxDeviceManager --msgid-bugs-address= \
    --sort-by-file --no-location -o po/qlinuxdevicemanager.pot
