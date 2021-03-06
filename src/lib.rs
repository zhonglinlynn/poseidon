extern crate pairing;
extern crate rand;
extern crate blake2_rfc_bellman_edition as blake2_rfc;
extern crate tiny_keccak;
extern crate byteorder;
extern crate alga;
extern crate approx;
extern crate mathru;

use crate::pairing::ff::{Field, PrimeField};
use crate::pairing::{Engine};
use std::marker::PhantomData;

use crate::rand::{Rng};

pub mod group_hash;
mod constants;
mod algebra;
pub mod specialization;

pub mod bn256;

pub trait SBox<E: Engine>: Sized + Clone {
    fn apply(&self, elements: &mut [E::Fr]);
}

#[derive(Clone)]
pub struct CubicSBox<E: Engine> {
    pub _marker: PhantomData<E>
}

impl<E: Engine>SBox<E> for CubicSBox<E> {
    fn apply(&self, elements: &mut [E::Fr]) {
        for element in elements.iter_mut() {
            let mut squared = *element;
            squared.square();
            element.mul_assign(&squared);
        }
    }
}

#[derive(Clone)]
pub struct QuinticSBox<E: Engine> {
    pub _marker: PhantomData<E>
}

impl<E: Engine>SBox<E> for QuinticSBox<E> {
    fn apply(&self, elements: &mut [E::Fr]) {
        for element in elements.iter_mut() {
            let mut quad = *element;
            quad.square();
            quad.square();
            element.mul_assign(&quad);
        }
    }
}

const POWER_SBOX_WINDOW_SIZE: usize = 4;

#[derive(Clone)]
pub struct PowerSBox<E: Engine> {
    pub power: <E::Fr as PrimeField>::Repr,
    pub precomputed_indexes: Vec<usize>,
    pub inv: u64,
}

impl<E: Engine>SBox<E> for PowerSBox<E> {
    fn apply(&self, elements: &mut [E::Fr]) {
        if self.precomputed_indexes.len() != 0 {
            let mut table = [E::Fr::zero(); 1 << POWER_SBOX_WINDOW_SIZE];
            table[0] = E::Fr::one();

            for element in elements.iter_mut() {
                let mut current = *element;
                table[1] = current;
                
                for i in 2..(1 << POWER_SBOX_WINDOW_SIZE) {
                    current.mul_assign(&*element);
                    table[i] = current;
                }

                let bound = self.precomputed_indexes.len() - 1;
                let mut result = table[self.precomputed_indexes[0]];
                for _ in 0..POWER_SBOX_WINDOW_SIZE {
                    result.square();
                }

                for i in 1..bound {
                    result.mul_assign(&table[self.precomputed_indexes[i]]);
                    for _ in 0..POWER_SBOX_WINDOW_SIZE {
                        result.square();
                    }
                }

                result.mul_assign(&table[self.precomputed_indexes[bound]]);

                *element = result;
            }
        } else {
            for element in elements.iter_mut() {
                *element = element.pow(&self.power);
            }
        }
    }
}

#[derive(Clone)]
pub struct InversionSBox<E: Engine> {
    pub _marker: PhantomData<E>
}

fn batch_inversion<E: Engine>(v: &mut [E::Fr]) {
    // Montgomery???s Trick and Fast Implementation of Masked AES
    // Genelle, Prouff and Quisquater
    // Section 3.2

    // First pass: compute [a, ab, abc, ...]
    let mut prod = Vec::with_capacity(v.len());
    let mut tmp = E::Fr::one();
    for g in v.iter()
        // Ignore zero elements
        .filter(|g| !g.is_zero())
    {
        tmp.mul_assign(&g);
        prod.push(tmp);
    }

    // Invert `tmp`.
    tmp = tmp.inverse().unwrap(); // Guaranteed to be nonzero.

    // Second pass: iterate backwards to compute inverses
    for (g, s) in v.iter_mut()
                    // Backwards
                    .rev()
                    // Ignore normalized elements
                    .filter(|g| !g.is_zero())
                    // Backwards, skip last element, fill in one for last term.
                    .zip(prod.into_iter().rev().skip(1).chain(Some(E::Fr::one())))
    {
        // tmp := tmp * g.z; g.z := tmp * s = 1/z
        let mut newtmp = tmp;
        newtmp.mul_assign(&g);
        *g = tmp;
        g.mul_assign(&s);
        tmp = newtmp;
    }
}

impl<E: Engine>SBox<E> for InversionSBox<E> {
    fn apply(&self, elements: &mut [E::Fr]) {
        batch_inversion::<E>(elements);
    }
}

pub trait PoseidonHashParams<E: Engine>: PoseidonParamsInternal<E> {
    type SBox: SBox<E>;
    fn capacity(&self) -> u32;
    fn rate(&self) -> u32;
    fn state_width(&self) -> u32 {
        self.capacity() + self.rate()
    }
    fn num_full_rounds(&self) -> u32;
    fn num_partial_rounds(&self) -> u32;
    fn round_constants(&self, round: u32) -> &[E::Fr];
    fn mds_matrix_row(&self, row: u32) -> &[E::Fr];
    fn security_level(&self) -> u32;
    fn output_len(&self) -> u32 {
        self.capacity()
    }
    fn absorbtion_cycle_len(&self) -> u32 {
        self.rate()
    }
    fn compression_rate(&self) -> u32 {
        self.absorbtion_cycle_len() / self.output_len()
    }

    fn sbox(&self) -> &Self::SBox;
}

pub trait PoseidonParamsInternal<E: Engine>: Send + Sync + Sized + Clone {
    fn set_round_constants(&mut self, to: Vec<E::Fr>);
}

pub trait PoseidonEngine: Engine {
    type Params: PoseidonHashParams<Self>; 
}

pub fn poseidon_hash<E: PoseidonEngine>(
    params: &E::Params,
    input: &[E::Fr]
) -> Vec<E::Fr> {
    sponge::<E>(params, input)
}

fn sponge<E: PoseidonEngine>(
    params: &E::Params,
    input: &[E::Fr]
) -> Vec<E::Fr> {

    let mut stateful = StatefulSponge::<E>::new(params);
    stateful.absorb(&input);

    let mut output = Vec::with_capacity(params.capacity() as usize);
    for _ in 0..params.capacity() {
        output.push(stateful.squeeze_out_single());
    }
    
    output
}   

pub fn poseidon_mimc<E: PoseidonEngine>(
    params: &E::Params,
    old_state: &[E::Fr]
) -> Vec<E::Fr> {
    let mut state = old_state.to_vec();
    debug_assert!(params.num_full_rounds() % 2 == 0);
    let half_of_full_rounds = params.num_full_rounds() / 2;
    let mut mds_application_scratch = vec![E::Fr::zero(); state.len()];
    assert_eq!(state.len(), params.state_width() as usize);

    let last_elem_idx = state.len() - 1;

    // full rounds
    for round in 0..half_of_full_rounds {
        let round_constants = params.round_constants(round);
    
        // add round constatnts
        for (s, c)  in state.iter_mut()
                    .zip(round_constants.iter()) {
            s.add_assign(c);
        }

        params.sbox().apply(&mut state[..]);

        // mul state by MDS
        for (row, place_into) in mds_application_scratch.iter_mut()
                                        .enumerate() {
            let tmp = scalar_product::<E>(& state[..], params.mds_matrix_row(row as u32));                           
            *place_into = tmp;
        }

        // place new data into the state
        state.copy_from_slice(&mds_application_scratch[..]);
    }

    // partial rounds

    for round in half_of_full_rounds..(params.num_partial_rounds() + half_of_full_rounds){
        let round_constants = params.round_constants(round);
    
        // add round constatnts
        for (s, c)  in state.iter_mut()
                    .zip(round_constants.iter()) {
            s.add_assign(c);
        }

        params.sbox().apply(&mut state[last_elem_idx..]);

        // mul state by MDS
        for (row, place_into) in mds_application_scratch.iter_mut()
                                        .enumerate() {
            let tmp = scalar_product::<E>(& state[..], params.mds_matrix_row(row as u32));
            *place_into = tmp;                               
        }

        // place new data into the state
        state.copy_from_slice(&mds_application_scratch[..]);
    }

    // full rounds
    for round in (params.num_partial_rounds() + half_of_full_rounds)..(params.num_partial_rounds() + params.num_full_rounds()) {
        let round_constants = params.round_constants(round);
    
        // add round constatnts
        for (s, c)  in state.iter_mut()
                    .zip(round_constants.iter()) {
            s.add_assign(c);
        }

        params.sbox().apply(&mut state[..]);

        // mul state by MDS
        for (row, place_into) in mds_application_scratch.iter_mut()
                                        .enumerate() {
            let tmp = scalar_product::<E>(& state[..], params.mds_matrix_row(row as u32));                           
            *place_into = tmp;
        }

        // place new data into the state
        state.copy_from_slice(&mds_application_scratch[..]);
    }

    state
}

#[inline]
fn scalar_product<E: Engine> (input: &[E::Fr], by: &[E::Fr]) -> E::Fr {
    debug_assert!(input.len() == by.len());
    let mut result = E::Fr::zero();
    for (a, b) in input.iter().zip(by.iter()) {
        let mut tmp = *a;
        tmp.mul_assign(b);
        result.add_assign(&tmp);
    }

    result
}

// For simplicity we'll not generate a matrix using a way from the paper and sampling
// an element with some zero MSBs and instead just sample and retry
fn generate_mds_matrix<E: PoseidonEngine, R: Rng>(t: u32, rng: &mut R) -> Vec<E::Fr> {
    loop {
        let x: Vec<E::Fr> = (0..t).map(|_| rng.gen()).collect();
        let y: Vec<E::Fr> = (0..t).map(|_| rng.gen()).collect();

        let mut invalid = false;

        // quick and dirty check for uniqueness of x
        for i in 0..(t as usize) {
            if invalid {
                continue;
            }
            let el = x[i];
            for other in x[(i+1)..].iter() {
                if el == *other {
                    invalid = true;
                    break;
                }
            }
        }

        if invalid {
            continue;
        }

        // quick and dirty check for uniqueness of y
        for i in 0..(t as usize) {
            if invalid {
                continue;
            }
            let el = y[i];
            for other in y[(i+1)..].iter() {
                if el == *other {
                    invalid = true;
                    break;
                }
            }
        }

        if invalid {
            continue;
        }

        // quick and dirty check for uniqueness of x vs y
        for i in 0..(t as usize) {
            if invalid {
                continue;
            }
            let el = x[i];
            for other in y.iter() {
                if el == *other {
                    invalid = true;
                    break;
                }
            }
        }

        if invalid {
            continue;
        }

        // by previous checks we can be sure in uniqueness and perform subtractions easily
        let mut mds_matrix = vec![E::Fr::zero(); (t*t) as usize];
        for (i, x) in x.into_iter().enumerate() {
            for (j, y) in y.iter().enumerate() {
                let place_into = i*(t as usize) + j;
                let mut element = x;
                element.sub_assign(y);
                mds_matrix[place_into] = element;
            }
        }

        // now we need to do the inverse
        batch_inversion::<E>(&mut mds_matrix[..]);

        return mds_matrix;
    }
}

// pub fn make_keyed_params<E: PoseidonEngine>(
//     default_params: &E::Params,
//     key: &[E::Fr]
// ) -> E::Params {
//     // for this purpose we feed the master key through the rescue itself
//     // in a sense that we make non-trivial initial state and run it with empty input

//     assert_eq!(default_params.state_width() as usize, key.len());

//     let mut new_round_constants = vec![];

//     let mut state = key.to_vec();
//     let mut mds_application_scratch = vec![E::Fr::zero(); state.len()];
//     assert_eq!(state.len(), default_params.state_width() as usize);
//     // add round constatnts
//     for (s, c)  in state.iter_mut()
//                 .zip(default_params.round_constants(0).iter()) {
//         s.add_assign(c);
//     }

//     // add to round constants
//     new_round_constants.extend_from_slice(&state);

//     // parameters use number of rounds that is number of invocations of each SBox,
//     // so we double
//     for round_num in 0..(2*default_params.num_rounds()) {
//         // apply corresponding sbox
//         if round_num & 1u32 == 0 {
//             default_params.sbox_0().apply(&mut state);
//         } else {
//             default_params.sbox_1().apply(&mut state);
//         }

//         // add round keys right away
//         mds_application_scratch.copy_from_slice(default_params.round_constants(round_num + 1));

//         // mul state by MDS
//         for (row, place_into) in mds_application_scratch.iter_mut()
//                                         .enumerate() {
//             let tmp = scalar_product::<E>(& state[..], default_params.mds_matrix_row(row as u32));
//             place_into.add_assign(&tmp);                                
//             // *place_into = scalar_product::<E>(& state[..], params.mds_matrix_row(row as u32));
//         }

//         // place new data into the state
//         state.copy_from_slice(&mds_application_scratch[..]);

//         new_round_constants.extend_from_slice(&state);
//     }
    
//     let mut new_params = default_params.clone();

//     new_params.set_round_constants(new_round_constants);

//     new_params
// }

#[derive(Clone)]
enum OpMode<E: PoseidonEngine> {
    AccumulatingToAbsorb(Vec<E::Fr>),
    SqueezedInto(Vec<E::Fr>)
}

pub struct StatefulSponge<'a, E: PoseidonEngine> {
    params: &'a E::Params,
    internal_state: Vec<E::Fr>,
    mode: OpMode<E>
}

impl<'a, E: PoseidonEngine> Clone for StatefulSponge<'a, E> {
    fn clone(&self) -> Self {
        Self {
            params: self.params,
            internal_state: self.internal_state.clone(),
            mode: self.mode.clone()
        }
    }
}

impl<'a, E: PoseidonEngine> StatefulSponge<'a, E> {
    pub fn new(
        params: &'a E::Params
    ) -> Self {
        let op = OpMode::AccumulatingToAbsorb(Vec::with_capacity(params.rate() as usize));

        Self {
            params,
            internal_state: vec![E::Fr::zero(); params.state_width() as usize],
            mode: op
        }
    }

    pub fn absorb_single_value(
        &mut self,
        value: E::Fr
    ) {
        match self.mode {
            OpMode::AccumulatingToAbsorb(ref mut into) => {
                // two cases
                // either we have accumulated enough already and should to 
                // a mimc round before accumulating more, or just accumulate more
                let rate = self.params.rate() as usize;
                if into.len() < rate {
                    into.push(value);
                } else {
                    for i in 0..rate {
                        self.internal_state[i].add_assign(&into[i]);
                    }

                    self.internal_state = poseidon_mimc::<E>(self.params, &self.internal_state);

                    into.truncate(0);
                    into.push(value);
                }
            },
            OpMode::SqueezedInto(_) => {
                // we don't need anything from the output, so it's dropped

                let mut s = Vec::with_capacity(self.params.rate() as usize);
                s.push(value);

                let op = OpMode::AccumulatingToAbsorb(s);
                self.mode = op;
            }
        }
    }

    pub fn absorb(
        &mut self,
        input: &[E::Fr]
    ) {
        let rate = self.params.rate() as usize;
        let mut absorbtion_cycles = input.len() / rate;
        if input.len() % rate != 0 {
            absorbtion_cycles += 1;
        }
        let padding_len = absorbtion_cycles * rate - input.len();
        let padding = vec![E::Fr::one(); padding_len];

        let it = input.iter().chain(&padding);

        for &val in it {
            self.absorb_single_value(val);
        }
    }

    pub fn squeeze_out_single(
        &mut self,
    ) -> E::Fr {
        match self.mode {
            OpMode::AccumulatingToAbsorb(ref mut into) => {
                let rate = self.params.rate() as usize;
                if into.len() < rate {
                    into.resize(rate, E::Fr::one());
                }

                assert_eq!(into.len(), rate, "padding was necessary!");
                // two cases
                // either we have accumulated enough already and should to 
                // a mimc round before accumulating more, or just accumulate more
                for i in 0..rate {
                    self.internal_state[i].add_assign(&into[i]);
                }
                self.internal_state = poseidon_mimc::<E>(self.params, &self.internal_state);

                // we don't take full internal state, but only the rate
                let mut sponge_output = self.internal_state[0..rate].to_vec();
                let output = sponge_output.drain(0..1).next().unwrap();

                let op = OpMode::SqueezedInto(sponge_output);
                self.mode = op;

                return output;
            },
            OpMode::SqueezedInto(ref mut into) => {
                if into.len() == 0 {
                    let rate = self.params.rate() as usize;

                    self.internal_state = poseidon_mimc::<E>(self.params, &self.internal_state);

                    let mut sponge_output = self.internal_state[0..rate].to_vec();
                    let output = sponge_output.drain(0..1).next().unwrap();
    
                    let op = OpMode::SqueezedInto(sponge_output);
                    self.mode = op;
    
                    return output;
                }

                assert!(into.len() > 0, "squeezed state is depleted!");
                let output = into.drain(0..1).next().unwrap();

                return output;
            }
        }
    }
}