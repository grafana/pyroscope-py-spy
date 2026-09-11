#!/bin/sh
set -eu

python --version
cat /etc/os-release

python -m zipfile -e /wheels/*.whl /tmp/py-spy-wheel
cp /tmp/py-spy-wheel/*.data/scripts/py-spy /usr/local/bin/py-spy
chmod +x /usr/local/bin/py-spy

test_wheel() {
    python tests/integration_test.py ||
        python tests/integration_test.py ||
        python tests/integration_test.py
}

test_wheel

if command -v apk >/dev/null; then
    apk add --no-cache binutils
else
    apt-get update
    apt-get install -y --no-install-recommends binutils
fi

python_binary=$(python -c 'import os, sys; print(os.path.realpath(sys.executable))')
python_library=$(python -c 'import os, sysconfig; print(os.path.join(sysconfig.get_config_var("LIBDIR"), sysconfig.get_config_var("LDLIBRARY")))')
strip --strip-all "$python_binary" "$python_library"
readelf --sections --wide "$python_library"
readelf --dyn-syms --wide "$python_library" | grep _PyRuntime || true

test_wheel
