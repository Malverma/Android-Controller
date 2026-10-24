//! QR-code pairing secrets (spec §3.4).

use rand::Rng;
use rand::distr::Alphanumeric;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrSecret {
    /// mDNS instance name the phone will advertise after scanning.
    pub name: String,
    pub password: String,
}

impl QrSecret {
    pub fn generate(rng: &mut impl Rng) -> Self {
        let mut alnum = |n| (0..n).map(|_| rng.sample(Alphanumeric) as char).collect::<String>();
        Self { name: format!("studio-{}", alnum(10)), password: alnum(12) }
    }

    /// The string Android's "Pair device with QR code" scanner expects.
    pub fn payload(&self) -> String {
        format!("WIFI:T:ADB;S:{};P:{};;", self.name, self.password)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_format() {
        let s = QrSecret { name: "studio-abc".into(), password: "pw".into() };
        assert_eq!(s.payload(), "WIFI:T:ADB;S:studio-abc;P:pw;;");
    }

    #[test]
    fn generated_secrets_are_alphanumeric_and_distinct() {
        let mut rng = rand::rng();
        let a = QrSecret::generate(&mut rng);
        let b = QrSecret::generate(&mut rng);
        assert_ne!(a, b);
        assert_eq!(a.name.len(), "studio-".len() + 10);
        assert_eq!(a.password.len(), 12);
        assert!(a.password.chars().all(|c| c.is_ascii_alphanumeric()));
    }
}
