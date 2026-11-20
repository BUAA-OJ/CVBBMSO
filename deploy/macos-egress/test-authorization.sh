#!/bin/bash
set -euo pipefail

cd -- "$(dirname -- "$0")/../.."
scratch=$(/usr/bin/mktemp -d)
trap '/bin/rm -rf -- "$scratch"' EXIT
cat > "$scratch/requirements.m" <<'EOF'
#include "macos_egress.m"

int main(void) {
    @autoreleasepool {
        if (requirement(AppID, "") || requirement(ServiceID, "bad-team!1")) return 1;
        for (NSString *identifier in @[AppID, ServiceID]) {
            NSString *text = requirement(identifier, "DEMO123456");
            if (!text) return 2;
            text = [text stringByAppendingString:@" and cdhash H\"0000000000000000000000000000000000000000\""];
            SecRequirementRef parsed = NULL;
            OSStatus status = SecRequirementCreateWithString((__bridge CFStringRef)text, kSecCSDefaultFlags, &parsed);
            if (status != errSecSuccess) {
                fprintf(stderr, "Invalid requirement for %s: OSStatus %d\n", identifier.UTF8String, (int)status);
                return 3;
            }
            CFRelease(parsed);
        }
    }
    return 0;
}
EOF
/usr/bin/xcrun --sdk macosx clang -fobjc-arc -Wall -Wextra -Werror \
    -I "$PWD/crates/cvbbmso-network/native" "$scratch/requirements.m" \
    -framework Foundation -framework Security -framework ServiceManagement -o "$scratch/requirements"
"$scratch/requirements"
/usr/bin/codesign --verify --test-requirement='=anchor apple' /usr/bin/true

cat > "$scratch/stock.conf" <<'EOF'
scrub-anchor "com.apple/*"
anchor "com.apple/*"
EOF
{
    head -1 "$scratch/stock.conf"
    cat deploy/macos-egress/pf-entry.conf
    tail -1 "$scratch/stock.conf"
} > "$scratch/candidate.conf"
/sbin/pfctl -n -R -o none -v -f "$scratch/stock.conf" > "$scratch/stock"
/sbin/pfctl -n -R -o none -v -f "$scratch/candidate.conf" > "$scratch/candidate"
/usr/bin/grep -Fx 'anchor "com.cvbbmso.egress" out all' "$scratch/candidate"
/usr/bin/grep -Fvx 'anchor "com.cvbbmso.egress" out all' "$scratch/candidate" > "$scratch/candidate-without-entry"
/usr/bin/diff -u "$scratch/stock" "$scratch/candidate-without-entry"
printf '%s\n' 'Apple signing requirements and parser-only PF filter change passed.'
