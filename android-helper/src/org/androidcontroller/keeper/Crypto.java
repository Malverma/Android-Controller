package org.androidcontroller.keeper;

import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.MessageDigest;
import java.security.PrivateKey;
import java.security.PublicKey;
import java.security.SecureRandom;
import java.security.interfaces.ECPublicKey;
import java.security.spec.ECGenParameterSpec;
import java.security.spec.ECParameterSpec;
import java.security.spec.ECPoint;
import java.security.spec.ECPublicKeySpec;

import javax.crypto.KeyAgreement;
import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * The few primitives the PC protocol needs: SHA-256, HMAC-SHA256 and P-256
 * ECDH. Public keys travel as uncompressed points (0x04 || X || Y).
 */
final class Crypto {
    private static final SecureRandom RANDOM = new SecureRandom();

    private Crypto() {}

    static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static byte[] sha256(byte[]... parts) {
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            for (byte[] p : parts) md.update(p);
            return md.digest();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException(e);
        }
    }

    static byte[] hmac(byte[] key, byte[]... parts) {
        try {
            Mac mac = Mac.getInstance("HmacSHA256");
            mac.init(new SecretKeySpec(key, "HmacSHA256"));
            for (byte[] p : parts) mac.update(p);
            return mac.doFinal();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException(e);
        }
    }

    /** Constant-time comparison. */
    static boolean same(byte[] a, byte[] b) {
        return a != null && b != null && MessageDigest.isEqual(a, b);
    }

    static byte[] random(int n) {
        byte[] b = new byte[n];
        RANDOM.nextBytes(b);
        return b;
    }

    static int randomInt(int bound) {
        return RANDOM.nextInt(bound);
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder(b.length * 2);
        for (byte x : b) s.append(Character.forDigit((x >> 4) & 15, 16)).append(Character.forDigit(x & 15, 16));
        return s.toString();
    }

    static byte[] unhex(String s) {
        if (s == null || s.length() % 2 != 0) throw new IllegalArgumentException("bad hex");
        byte[] b = new byte[s.length() / 2];
        for (int i = 0; i < b.length; i++) {
            int hi = Character.digit(s.charAt(2 * i), 16), lo = Character.digit(s.charAt(2 * i + 1), 16);
            if (hi < 0 || lo < 0) throw new IllegalArgumentException("bad hex");
            b[i] = (byte) ((hi << 4) | lo);
        }
        return b;
    }

    static KeyPair ecKeyPair() throws GeneralSecurityException {
        KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
        g.initialize(new ECGenParameterSpec("secp256r1"), RANDOM);
        return g.generateKeyPair();
    }

    static byte[] encodePoint(PublicKey key) {
        ECPoint w = ((ECPublicKey) key).getW();
        byte[] out = new byte[65];
        out[0] = 4;
        fixed32(w.getAffineX(), out, 1);
        fixed32(w.getAffineY(), out, 33);
        return out;
    }

    /** The peer's point on the same curve as {@code ours}. */
    static PublicKey decodePoint(byte[] b, PublicKey ours) throws GeneralSecurityException {
        if (b.length != 65 || b[0] != 4) throw new GeneralSecurityException("bad public key");
        byte[] x = new byte[32], y = new byte[32];
        System.arraycopy(b, 1, x, 0, 32);
        System.arraycopy(b, 33, y, 0, 32);
        ECParameterSpec params = ((ECPublicKey) ours).getParams();
        ECPoint p = new ECPoint(new BigInteger(1, x), new BigInteger(1, y));
        return KeyFactory.getInstance("EC").generatePublic(new ECPublicKeySpec(p, params));
    }

    /** The shared secret: the X coordinate, 32 bytes. */
    static byte[] ecdh(PrivateKey mine, PublicKey theirs) throws GeneralSecurityException {
        KeyAgreement ka = KeyAgreement.getInstance("ECDH");
        ka.init(mine);
        ka.doPhase(theirs, true);
        byte[] s = ka.generateSecret();
        if (s.length == 32) return s;
        byte[] out = new byte[32];
        fixed32(new BigInteger(1, s), out, 0);
        return out;
    }

    private static void fixed32(BigInteger v, byte[] out, int off) {
        byte[] b = v.toByteArray();
        int n = Math.min(b.length, 32);
        System.arraycopy(b, b.length - n, out, off + 32 - n, n);
    }
}
