#!/bin/bash
set -e  # exit on first failure

S3="http://100.100.30.59:9000"
WEBDAV="http://100.100.30.59:8080"
PASS=0
FAIL=0

check() {
    local name="$1"
    if [ "$2" = "0" ]; then
        echo "  ✅ $name"
        PASS=$((PASS+1))
    else
        echo "  ❌ $name"
        FAIL=$((FAIL+1))
    fi
}

check_code() {
    local name="$1" actual="$2" expected="$3"
    if [ "$actual" = "$expected" ]; then
        check "$name" 0
    else
        echo "     expected HTTP $expected, got $actual"
        check "$name" 1
    fi
}

echo "=== Integration Test: WebDAV Pipeline ==="
echo ""

# ======= Setup: create test bucket =======
echo "--- Setup ---"
BUCKET="inttest-webdav-$$"
mkdir -p /tmp/inttest-$$

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET")
check_code "Create bucket $BUCKET" "$CODE" "200"

# ======= Generate test files =======
echo "--- Generate test files ---"

# Small file: 128 KB
python3 -c "
import struct
data = bytes((i ^ (i>>8)) & 0xFF for i in range(128*1024))
open('/tmp/inttest-$$/small.bin','wb').write(data)
" 

# Large file: 33 MB (over 32 MB chunk threshold)
python3 -c "
data = bytes(((i*17+31) % 256) for i in range(33*1024*1024))
open('/tmp/inttest-$$/large.bin','wb').write(data)
"

# Compute checksums
SMALL_HASH=$(sha256sum /tmp/inttest-$$/small.bin | awk '{print $1}')
LARGE_HASH=$(sha256sum /tmp/inttest-$$/large.bin | awk '{print $1}')
echo "  Small file: $(stat -c%s /tmp/inttest-$$/small.bin) bytes, SHA256: $SMALL_HASH"
echo "  Large file: $(stat -c%s /tmp/inttest-$$/large.bin) bytes, SHA256: $LARGE_HASH"

# ======= Upload via S3 =======
echo "--- Upload via S3 ---"

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET/test-folder/small.bin" --data-binary @/tmp/inttest-$$/small.bin)
check_code "Upload small.bin" "$CODE" "200"

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X PUT "$S3/$BUCKET/test-folder/large.bin" --data-binary @/tmp/inttest-$$/large.bin)
check_code "Upload large.bin" "$CODE" "200"

# ======= List folder via WebDAV =======
echo "--- WebDAV: List folder ---"

RESULT=$(curl -s -X PROPFIND "$WEBDAV/$BUCKET/test-folder/" -H "Depth: 1")
echo "$RESULT" | grep -q "small.bin" && check "List shows small.bin" 0 || check "List shows small.bin" 1
echo "$RESULT" | grep -q "large.bin" && check "List shows large.bin" 0 || check "List shows large.bin" 1
echo "$RESULT" | grep -q "getcontentlength" && check "List includes sizes" 0 || check "List includes sizes" 1

# ======= Download via WebDAV and verify =======
echo "--- WebDAV: Download and verify ---"

# Small file
curl -s "$WEBDAV/$BUCKET/test-folder/small.bin" -o /tmp/inttest-$$/small-dl.bin
DL_SMALL_HASH=$(sha256sum /tmp/inttest-$$/small-dl.bin | awk '{print $1}')
if [ "$DL_SMALL_HASH" = "$SMALL_HASH" ]; then
    check "Small file SHA256 match" 0
else
    check "Small file SHA256 match (expected $SMALL_HASH, got $DL_SMALL_HASH)" 1
fi

# Small file: compare bytes
cmp /tmp/inttest-$$/small.bin /tmp/inttest-$$/small-dl.bin && check "Small file content exact match" 0 || check "Small file content exact match" 1

# Large file: range request (first 100 bytes)
curl -s -H "Range: bytes=0-99" "$WEBDAV/$BUCKET/test-folder/large.bin" -o /tmp/inttest-$$/large-range.bin
RANGE_SIZE=$(stat -c%s /tmp/inttest-$$/large-range.bin)
[ "$RANGE_SIZE" = "100" ] && check "Large file range: 100 bytes" 0 || check "Large file range: expected 100 bytes, got $RANGE_SIZE" 1

# Large file: full download
echo "  Downloading 33 MB file via WebDAV (may take a moment)..."
curl -s "$WEBDAV/$BUCKET/test-folder/large.bin" -o /tmp/inttest-$$/large-dl.bin
DL_LARGE_HASH=$(sha256sum /tmp/inttest-$$/large-dl.bin | awk '{print $1}')
if [ "$DL_LARGE_HASH" = "$LARGE_HASH" ]; then
    check "Large file SHA256 match" 0
else
    check "Large file SHA256 match (expected $LARGE_HASH, got $DL_LARGE_HASH)" 1
fi

DL_SIZE=$(stat -c%s /tmp/inttest-$$/large-dl.bin)
EXPECTED=$((33*1024*1024))
[ "$DL_SIZE" = "$EXPECTED" ] && check "Large file size: $EXPECTED bytes" 0 || check "Large file size: expected $EXPECTED, got $DL_SIZE" 1

# ======= Clean up =======
echo "--- Clean up ---"

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$S3/$BUCKET/test-folder/small.bin")
check_code "Delete small.bin" "$CODE" "204"

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$S3/$BUCKET/test-folder/large.bin")
check_code "Delete large.bin" "$CODE" "204"

CODE=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE "$S3/$BUCKET")
check_code "Delete bucket $BUCKET" "$CODE" "204"

rm -rf /tmp/inttest-$$

# ======= Summary =======
echo ""
echo "=== Results: $PASS passed, $FAIL failed ==="
[ "$FAIL" -eq 0 ] && echo "✅ All tests passed!" || echo "❌ Some tests failed"
exit $FAIL
