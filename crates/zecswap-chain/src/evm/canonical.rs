use std::iter;

use alloy::sol_types::SolCall;

use super::U256;

#[derive(Debug)]
pub enum Shape {
    Words(usize),
    Bool,
    Bytes(usize),
    Array(&'static Shape, usize),
    Capping(&'static Shape, usize),
    Capped(&'static Shape),
    Tuple(&'static [Shape]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Malformed {
    NotTheCall,
    Noncanonical,
    TooLong,
}

pub fn decode<C: SolCall>(data: &[u8], args: &[Shape]) -> Result<C, Malformed> {
    let data = data
        .strip_prefix(&C::SELECTOR)
        .ok_or(Malformed::NotTheCall)?;
    let end = Walk { data, cap: 0 }.tuple(0, args.iter())?;
    if end != data.len() {
        return Err(Malformed::Noncanonical);
    }
    C::abi_decode_raw_validate(data).map_err(|_| Malformed::NotTheCall)
}

impl Shape {
    fn dynamic(&self) -> bool {
        match self {
            Shape::Words(_) | Shape::Bool => false,
            Shape::Tuple(fields) => fields.iter().any(Shape::dynamic),
            _ => true,
        }
    }

    fn head(&self) -> usize {
        match self {
            Shape::Words(words) => 32 * words,
            Shape::Tuple(fields) if !self.dynamic() => fields.iter().map(Shape::head).sum(),
            _ => 32,
        }
    }
}

struct Walk<'a> {
    data: &'a [u8],
    cap: usize,
}

impl Walk<'_> {
    fn tuple<'s>(
        &mut self,
        start: usize,
        fields: impl Iterator<Item = &'s Shape> + Clone,
    ) -> Result<usize, Malformed> {
        let mut head = start;
        let mut end = start + fields.clone().map(Shape::head).sum::<usize>();
        for field in fields {
            if field.dynamic() {
                if self.word(head)? != U256::from(end - start) {
                    return Err(Malformed::Noncanonical);
                }
                end = self.value(end, field)?;
            } else {
                self.inline(head, field)?;
            }
            head += field.head();
        }
        Ok(end)
    }

    fn value(&mut self, at: usize, shape: &Shape) -> Result<usize, Malformed> {
        let elements = at + 32;
        match *shape {
            Shape::Bytes(max) => {
                let len = self.len(at, max)?;
                let end = elements + len.next_multiple_of(32);
                let padded = self
                    .data
                    .get(elements..end)
                    .ok_or(Malformed::Noncanonical)?;
                if padded[len..].iter().any(|&byte| byte != 0) {
                    return Err(Malformed::Noncanonical);
                }
                Ok(end)
            }
            Shape::Array(of, max) => {
                let len = self.len(at, max)?;
                self.tuple(elements, iter::repeat_n(of, len))
            }
            Shape::Capping(of, max) => {
                let len = self.len(at, max)?;
                self.cap = len;
                self.tuple(elements, iter::repeat_n(of, len))
            }
            Shape::Capped(of) => {
                let len = self.len(at, self.cap)?;
                self.tuple(elements, iter::repeat_n(of, len))
            }
            Shape::Tuple(fields) => self.tuple(at, fields.iter()),
            Shape::Words(_) | Shape::Bool => unreachable!("static values are in place"),
        }
    }

    fn inline(&self, at: usize, shape: &Shape) -> Result<(), Malformed> {
        match shape {
            Shape::Bool if self.word(at)? > U256::from(1) => Err(Malformed::Noncanonical),
            Shape::Tuple(fields) => fields
                .iter()
                .try_fold(at, |at, field| {
                    self.inline(at, field)?;
                    Ok(at + field.head())
                })
                .map(drop),
            _ => Ok(()),
        }
    }

    fn len(&self, at: usize, max: usize) -> Result<usize, Malformed> {
        let len = self.word(at)?;
        if len > U256::from(max) {
            return Err(Malformed::TooLong);
        }
        Ok(len.to())
    }

    fn word(&self, at: usize) -> Result<U256, Malformed> {
        self.data
            .get(at..)
            .and_then(|rest| rest.get(..32))
            .map(U256::from_be_slice)
            .ok_or(Malformed::Noncanonical)
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;
    use alloy::sol;

    use super::*;

    sol! {
        struct Note { bytes memo; uint8 kind; }
        function take(bytes32[] words, Note[] notes, bool flag) external;
    }

    const NOTE: Shape = Shape::Tuple(&[Shape::Bytes(40), Shape::Words(1)]);
    const TAKE: &[Shape] = &[
        Shape::Capping(&Shape::Words(1), 3),
        Shape::Capped(&NOTE),
        Shape::Bool,
    ];

    fn call(words: usize, memos: &[usize]) -> takeCall {
        takeCall {
            words: vec![B256::repeat_byte(7); words],
            notes: memos
                .iter()
                .map(|&len| Note {
                    memo: vec![0xab; len].into(),
                    kind: 1,
                })
                .collect(),
            flag: true,
        }
    }

    fn calldata() -> Vec<u8> {
        call(2, &[5, 33]).abi_encode()
    }

    fn patched(words: &[(usize, usize)]) -> Vec<u8> {
        let mut data = calldata();
        for &(word, value) in words {
            let at = 4 + 32 * word;
            data[at..at + 32].copy_from_slice(&U256::from(value).to_be_bytes::<32>());
        }
        data
    }

    #[test]
    fn takes_canonical_encodings() {
        for (words, memos) in [
            (0, &[][..]),
            (1, &[0][..]),
            (2, &[31, 32][..]),
            (3, &[33, 40, 1][..]),
        ] {
            let call = call(words, memos);
            let data = call.abi_encode();
            assert_eq!(
                decode::<takeCall>(&data, TAKE).map(|decoded| decoded.abi_encode()),
                Ok(data)
            );
        }
    }

    #[test]
    fn refuses_values_sharing_or_skipping_bytes() {
        let cases: [(&str, &[(usize, usize)]); 5] = [
            ("both notes in the same bytes", &[(8, 0x40)]),
            ("the notes in reverse order", &[(7, 0xc0), (8, 0x40)]),
            ("a gap before the second note", &[(8, 0xe0)]),
            ("the notes in the words' bytes", &[(1, 0x60)]),
            ("a memo in its note's head", &[(9, 0x20)]),
        ];
        for (what, words) in cases {
            assert_eq!(
                decode::<takeCall>(&patched(words), TAKE).err(),
                Some(Malformed::Noncanonical),
                "{what}"
            );
        }
    }

    #[test]
    fn refuses_lengths_over_their_caps() {
        let over = [
            ("four words", call(4, &[1, 1]).abi_encode()),
            ("more notes than words", call(1, &[1, 1]).abi_encode()),
            ("a memo of 41 bytes", call(2, &[1, 41]).abi_encode()),
        ];
        for (what, data) in over {
            assert_eq!(
                decode::<takeCall>(&data, TAKE).err(),
                Some(Malformed::TooLong),
                "{what}"
            );
        }
        let mut huge = calldata();
        huge[4 + 3 * 32] = 1;
        assert_eq!(
            decode::<takeCall>(&huge, TAKE).err(),
            Some(Malformed::TooLong)
        );
    }

    #[test]
    fn refuses_bytes_past_the_end_and_short_calldata() {
        let mut trailing = calldata();
        trailing.extend_from_slice(&[0; 32]);
        let short = &calldata()[..calldata().len() - 1];
        for data in [&trailing[..], short] {
            assert_eq!(
                decode::<takeCall>(data, TAKE).err(),
                Some(Malformed::Noncanonical)
            );
        }
    }

    #[test]
    fn refuses_dirty_padding_and_bools_but_one_and_zero() {
        let mut padding = calldata();
        *padding.last_mut().unwrap() = 1;
        for data in [padding, patched(&[(2, 2)])] {
            assert_eq!(
                decode::<takeCall>(&data, TAKE).err(),
                Some(Malformed::Noncanonical)
            );
        }
    }

    #[test]
    fn refuses_other_calls_and_values_out_of_range() {
        let mut selector = calldata();
        selector[0] ^= 1;
        let kind = patched(&[(10, 256)]);
        for data in [selector, kind] {
            assert_eq!(
                decode::<takeCall>(&data, TAKE).err(),
                Some(Malformed::NotTheCall)
            );
        }
    }
}
