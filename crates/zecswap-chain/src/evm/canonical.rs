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
