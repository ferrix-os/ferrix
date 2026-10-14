//! The fixed crate the game path's background load compiles in a loop on
//! every processor (docs/GAME-PATH.md §2.4, stage 24's R3).
//!
//! It has no dependencies and needs no linker: `load.sh` builds it as an
//! rlib, so the work is rustc's alone and the same on both guests. What it
//! computes does not matter; how long it takes to compile does. Generic
//! code instantiated for several types, iterator chains and a macro that
//! stamps the same module out many times keep LLVM busy for some seconds
//! a build at `-C opt-level=2`. It is never run and never changes: a
//! different crate is a different load, and the record says which one ran.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::ops::{Add, Mul, Sub};

pub trait Number: Copy + Debug + Default + PartialOrd + Add<Output = Self> + Sub<Output = Self> + Mul<Output = Self> {
    fn from_u32(value: u32) -> Self;
}

impl Number for u32 {
    fn from_u32(value: u32) -> Self {
        value
    }
}
impl Number for u64 {
    fn from_u32(value: u32) -> Self {
        u64::from(value)
    }
}
impl Number for i64 {
    fn from_u32(value: u32) -> Self {
        i64::from(value)
    }
}
impl Number for f32 {
    fn from_u32(value: u32) -> Self {
        value as f32
    }
}
impl Number for f64 {
    fn from_u32(value: u32) -> Self {
        f64::from(value)
    }
}

macro_rules! stamp {
    ($($name:ident)*) => {$(
        pub mod $name {
            use super::*;

            #[derive(Debug, Clone, Default)]
            pub struct Grid<T: Number> {
                pub cells: Vec<T>,
                pub width: usize,
            }

            impl<T: Number> Grid<T> {
                pub fn new(width: usize, height: usize) -> Self {
                    let cells = (0..width * height)
                        .map(|i| T::from_u32((i as u32).wrapping_mul(2654435761) >> 7))
                        .collect();
                    Grid { cells, width }
                }

                pub fn blur(&self) -> Self {
                    let height = self.cells.len() / self.width.max(1);
                    let cells = (0..self.cells.len())
                        .map(|i| {
                            let (x, y) = (i % self.width, i / self.width);
                            [(0isize, 0isize), (-1, 0), (1, 0), (0, -1), (0, 1)]
                                .iter()
                                .filter_map(|&(dx, dy)| {
                                    let nx = x as isize + dx;
                                    let ny = y as isize + dy;
                                    (nx >= 0 && ny >= 0 && (nx as usize) < self.width && (ny as usize) < height)
                                        .then(|| self.cells[ny as usize * self.width + nx as usize])
                                })
                                .fold(T::default(), |a, b| a + b)
                        })
                        .collect();
                    Grid { cells, width: self.width }
                }

                pub fn histogram(&self, buckets: usize) -> BTreeMap<usize, usize> {
                    let mut out = BTreeMap::new();
                    for (i, cell) in self.cells.iter().enumerate() {
                        let key = format!("{cell:?}").len() * 31 + i % buckets.max(1);
                        *out.entry(key % buckets.max(1)).or_insert(0) += 1;
                    }
                    out
                }

                pub fn largest(&self, n: usize) -> Vec<(usize, T)> {
                    let mut all: Vec<(usize, T)> = self.cells.iter().copied().enumerate().collect();
                    all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                    all.into_iter().take(n).collect()
                }

                pub fn product(&self, other: &Self) -> T {
                    self.cells
                        .iter()
                        .zip(other.cells.iter())
                        .map(|(&a, &b)| a * b)
                        .fold(T::default(), |a, b| a + b)
                }

                pub fn differences(&self) -> Vec<T> {
                    self.cells.windows(2).map(|w| if w[1] > w[0] { w[1] - w[0] } else { w[0] - w[1] }).collect()
                }
            }

            pub fn words(text: &str) -> HashMap<String, usize> {
                let mut out = HashMap::new();
                for word in text
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| !w.is_empty())
                    .map(str::to_lowercase)
                {
                    *out.entry(word).or_insert(0) += 1;
                }
                out
            }

            pub fn all<T: Number>(size: usize) -> (T, Vec<(usize, T)>, BTreeMap<usize, usize>, Vec<T>) {
                let grid = Grid::<T>::new(size, size);
                let blurred = grid.blur().blur();
                (grid.product(&blurred), blurred.largest(8), blurred.histogram(16), grid.differences())
            }

            pub fn every_type(size: usize) -> String {
                format!(
                    "{:?}{:?}{:?}{:?}{:?}{:?}",
                    all::<u32>(size),
                    all::<u64>(size),
                    all::<i64>(size),
                    all::<f32>(size),
                    all::<f64>(size),
                    words(stringify!($name)),
                )
            }
        }
    )*};
}

stamp! {
    m00 m01 m02 m03 m04 m05 m06 m07 m08 m09 m10 m11 m12 m13 m14 m15
}

/// Every module's work, so that none of it is dead to the optimiser.
pub fn everything(size: usize) -> usize {
    [
        m00::every_type(size), m01::every_type(size), m02::every_type(size), m03::every_type(size),
        m04::every_type(size), m05::every_type(size), m06::every_type(size), m07::every_type(size),
        m08::every_type(size), m09::every_type(size), m10::every_type(size), m11::every_type(size),
        m12::every_type(size), m13::every_type(size), m14::every_type(size), m15::every_type(size),
    ]
    .iter()
    .map(String::len)
    .sum()
}
