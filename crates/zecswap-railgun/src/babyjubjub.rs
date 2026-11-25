use ark_bn254::Fr;
use ark_ff::{Field, MontFp};
use bloock_blake_rs::Blake512;

const A: Fr = MontFp!("168700");
const D: Fr = MontFp!("168696");
const BASE8: Point = Point {
    x: MontFp!("5299619240641551281634865583518297030282874472190772894086521144482721001553"),
    y: MontFp!("16950150798460657717958625567821834550301663161624707787222815936182638968203"),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Point {
    pub x: Fr,
    pub y: Fr,
}

impl Point {
    const IDENTITY: Point = Point {
        x: MontFp!("0"),
        y: MontFp!("1"),
    };

    fn add(&self, other: &Point) -> Point {
        let xy = self.x * other.x * self.y * other.y;
        let x = (self.x * other.y + self.y * other.x) / (Fr::ONE + D * xy);
        let y = (self.y * other.y - A * self.x * other.x) / (Fr::ONE - D * xy);
        Point { x, y }
    }
}

pub(crate) fn public_key(private_key: &[u8; 32]) -> Point {
    let mut blake = Blake512::default();
    blake.write(private_key);
    let hash = blake.sum(&[]);
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&hash[..32]);
    scalar[0] &= 0xf8;
    scalar[31] &= 0x7f;
    scalar[31] |= 0x40;

    let bit = |i: usize| (scalar[i / 8] >> (i % 8)) & 1 == 1;
    (3..=254).rev().fold(Point::IDENTITY, |acc, i| {
        let doubled = acc.add(&acc);
        if bit(i) { doubled.add(&BASE8) } else { doubled }
    })
}
