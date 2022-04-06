// TODO Remove th gadgetis
#![allow(missing_docs)]
// TODO Remove this
#![allow(unused_imports)]

use crate::gadget::is_zero::{IsZeroChip, IsZeroConfig, IsZeroInstruction};
use ecc::{EccConfig, GeneralEccChip};
use ecdsa::ecdsa::{AssignedEcdsaSig, AssignedPublicKey, EcdsaChip, EcdsaConfig};
use group::{ff::Field, prime::PrimeCurveAffine, Curve};
use halo2_proofs::{
    arithmetic::{BaseExt, CurveAffine},
    circuit::{AssignedCell, Layouter, Region, SimpleFloorPlanner},
    plonk::{
        Advice, Circuit, Column, ConstraintSystem, Error, Expression, Fixed, Instance, Selector,
        VirtualCells,
    },
    poly::Rotation,
};
use integer::{
    AssignedInteger, IntegerConfig, IntegerInstructions, WrongExt, NUMBER_OF_LOOKUP_LIMBS,
};
use keccak256::plain::Keccak;
use maingate::{
    Assigned, MainGate, MainGateConfig, RangeChip, RangeConfig, RangeInstructions, RegionCtx,
};
use pairing::arithmetic::FieldExt;
use secp256k1::Secp256k1Affine;
use std::convert::TryInto;
use std::{io::Cursor, marker::PhantomData, os::unix::prelude::FileTypeExt};

// TODO: Move these utils outside of `evm_circuit` so that they can be used by
// other circuits without crossing boundaries.
use crate::evm_circuit::util::{
    and, constraint_builder::BaseConstraintBuilder, not, or, select, RandomLinearCombination, Word,
};
use crate::util::Expr;

const POW_RAND_SIZE: usize = 63;

/// Auxiliary Gadget to verify a that a message hash is signed by the public
/// key corresponding to an Ethereum Address.
#[derive(Default, Debug)]
struct SignVerifyChip<F: FieldExt> {
    aux_generator: Secp256k1Affine,
    window_size: usize,
    _marker: PhantomData<F>,
    // ecdsa_chip: EcdsaChip<Secp256k1Affine, F>,
}

const KECCAK_IS_ENABLED: usize = 0;
const KECCAK_INPUT_RLC: usize = 1;
const KECCAK_INPUT_LEN: usize = 2;
const KECCAK_OUTPUT_RLC: usize = 3;

const BIT_LEN_LIMB: usize = 72;

/// Enable copy constraint between `src` integer limbs and `dst` limbs.  Then
/// assign the `dst` limbs values from `src`.
fn copy_integer<F: FieldExt, W: WrongExt>(
    region: &mut Region<'_, F>,
    name: &str,
    src: AssignedInteger<W, F>,
    dst: &[Column<Advice>; 4],
    offset: usize,
) -> Result<(), Error> {
    for (i, limb) in src.limbs().iter().enumerate() {
        let assigned_cell = region.assign_advice(
            || format!("{} limb {}", name, i),
            dst[i],
            offset,
            || limb.value().clone().ok_or(Error::Synthesis),
        )?;
        region.constrain_equal(assigned_cell.cell(), limb.cell())?;
    }
    Ok(())
}

fn assign_integer_bytes_le<F: FieldExt, W: BaseExt>(
    region: &mut Region<'_, F>,
    name: &str,
    src: W,
    dst: &[Column<Advice>],
    offset: usize,
) -> Result<(), Error> {
    let mut src_le = [0u8; 32];
    src.write(&mut Cursor::new(&mut src_le[..])).unwrap();
    for (i, byte) in src_le.iter().enumerate() {
        region.assign_advice(
            || format!("{} byte {}", name, i),
            dst[i],
            offset,
            || Ok(F::from(*byte as u64)),
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct SignVerifyConfig<F: FieldExt> {
    q_enable: Selector,
    pub_key_hash: [Column<Advice>; 32],
    address: Column<Advice>,
    msg_hash_rlc: Column<Advice>,
    msg_hash_rlc_is_zero: IsZeroConfig<F>,
    msg_hash_rlc_inv: Column<Advice>,

    // ECDSA
    main_gate_config: MainGateConfig,
    range_config: RangeConfig,
    // First 32 cells are coord x in little endian, following 32 cells are coord y in little
    // endian.
    pub_key: [Column<Advice>; 32 * 2],
    pk_x_limbs: [Column<Advice>; 4],
    pk_y_limbs: [Column<Advice>; 4],
    msg_hash: [Column<Advice>; 32],
    msg_hash_limbs: [Column<Advice>; 4],
    // signature: [[Column<Advice>; 32]; 2],
    power_of_randomness: [Column<Instance>; POW_RAND_SIZE],

    // [is_enabled, input_rlc, input_len, output_rlc]
    keccak_table: [Column<Advice>; 4],
}

struct KeccakAux {
    input: [u8; 64],
    output: [u8; 32],
}

impl<F: FieldExt> SignVerifyConfig<F> {
    pub fn load_range(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        let bit_len_lookup = BIT_LEN_LIMB / NUMBER_OF_LOOKUP_LIMBS;
        let range_chip = RangeChip::<F>::new(self.range_config.clone(), bit_len_lookup);
        range_chip.load_limb_range_table(layouter)?;
        range_chip.load_overflow_range_tables(layouter)?;

        Ok(())
    }

    pub fn load_keccak(
        &self,
        layouter: &mut impl Layouter<F>,
        auxs: Vec<KeccakAux>,
        randomness: F,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "keccak table",
            |mut region| {
                let mut offset = 0;

                // All zero row to allow simulating a disabled lookup.
                for (name, column, value) in &[
                    ("is_enabled", self.keccak_table[0], F::zero()),
                    ("input_rlc", self.keccak_table[1], F::zero()),
                    ("input_len", self.keccak_table[2], F::zero()),
                    ("output_rlc", self.keccak_table[3], F::zero()),
                ] {
                    region.assign_advice(
                        || format!("Keccak table assign {} {}", name, offset),
                        *column,
                        offset,
                        || Ok(*value),
                    )?;
                }
                offset += 1;

                for aux in &auxs {
                    let KeccakAux { input, output } = aux;
                    let input_rlc =
                        RandomLinearCombination::random_linear_combine(input.clone(), randomness);
                    let output_rlc = Word::random_linear_combine(output.clone(), randomness);
                    println!(
                        "DBG keccak [{:?}, {:}, {:?}]",
                        input_rlc,
                        input.len(),
                        output_rlc
                    );
                    for (name, column, value) in &[
                        ("is_enabled", self.keccak_table[0], F::one()),
                        ("input_rlc", self.keccak_table[1], input_rlc),
                        (
                            "input_len",
                            self.keccak_table[2],
                            F::from(input.len() as u64),
                        ),
                        ("output_rlc", self.keccak_table[3], output_rlc),
                    ] {
                        region.assign_advice(
                            || format!("Keccak table assign {} {}", name, offset),
                            *column,
                            offset,
                            || Ok(*value),
                        )?;
                    }
                    offset += 1;
                }
                Ok(())
            },
        )?;
        Ok(())
    }

    pub fn ecc_chip_config(&self) -> EccConfig {
        EccConfig::new(self.range_config.clone(), self.main_gate_config.clone())
    }

    pub fn integer_chip_config(&self) -> IntegerConfig {
        IntegerConfig::new(self.range_config.clone(), self.main_gate_config.clone())
    }
}

impl<F: FieldExt> SignVerifyChip<F> {
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        power_of_randomness: [Column<Instance>; POW_RAND_SIZE],
    ) -> SignVerifyConfig<F> {
        let q_enable = meta.complex_selector();

        let pub_key = [(); 32 * 2].map(|_| meta.advice_column());
        let pk_x_limbs = [(); 4].map(|_| meta.advice_column());
        pk_x_limbs.map(|c| meta.enable_equality(c));
        let pk_y_limbs = [(); 4].map(|_| meta.advice_column());
        pk_y_limbs.map(|c| meta.enable_equality(c));
        let msg_hash = [(); 32].map(|_| meta.advice_column());
        let msg_hash_limbs = [(); 4].map(|_| meta.advice_column());
        msg_hash_limbs.map(|c| meta.enable_equality(c));

        // create address, msg_hash, pub_key_hash, and msg_hash_inv, and iz_zero

        let address = meta.advice_column();
        let pub_key_hash = [(); 32].map(|_| meta.advice_column());

        let msg_hash_rlc = meta.advice_column();

        // is_enabled === msg_hash_rlc != 0

        let msg_hash_rlc_inv = meta.advice_column();
        let msg_hash_rlc_is_zero = IsZeroChip::configure(
            meta,
            |meta| meta.query_selector(q_enable),
            |meta| meta.query_advice(msg_hash_rlc, Rotation::cur()),
            msg_hash_rlc_inv,
        );
        let is_not_padding = not::expr(msg_hash_rlc_is_zero.is_zero_expression.clone());

        // lookup keccak table

        let keccak_table = [(); 4].map(|_| meta.advice_column());
        // let pow_rand_cols = [(); POW_RAND_SIZE].map(|_| meta.instance_column());

        // keccak lookup
        meta.lookup_any("keccak", |meta| {
            let q_enable = meta.query_selector(q_enable);
            let selector = q_enable * is_not_padding.clone();
            let mut table_map = Vec::new();

            let power_of_randomness =
                power_of_randomness.map(|c| meta.query_instance(c, Rotation::cur()));

            // Column 0: is_enabled
            let keccak_is_enabled =
                meta.query_advice(keccak_table[KECCAK_IS_ENABLED], Rotation::cur());
            table_map.push((selector.clone(), keccak_is_enabled));

            // Column 1: input_rlc (pub_key_rlc)
            let keccak_input_rlc =
                meta.query_advice(keccak_table[KECCAK_INPUT_RLC], Rotation::cur());
            let mut pub_key_be = pub_key.map(|c| meta.query_advice(c, Rotation::cur()));
            pub_key_be[..32].reverse();
            pub_key_be[32..].reverse();
            let pub_key_rlc = RandomLinearCombination::random_linear_combine_expr(
                pub_key_be,
                &power_of_randomness,
            );
            // DBG
            // let pub_key_rlc = power_of_randomness[..31]
            //     .iter()
            //     .fold(0.expr(), |acc, val| acc * 256.expr() + val.clone());
            table_map.push((selector.clone() * pub_key_rlc, keccak_input_rlc));

            // Column 2: input_len (64)
            let keccak_input_len =
                meta.query_advice(keccak_table[KECCAK_INPUT_LEN], Rotation::cur());
            table_map.push((selector.clone() * 64usize.expr(), keccak_input_len));

            // Column 3: output_rlc (pub_key_hash_rlc)
            let keccak_output_rlc =
                meta.query_advice(keccak_table[KECCAK_OUTPUT_RLC], Rotation::cur());
            let pub_key_hash = pub_key_hash.map(|c| meta.query_advice(c, Rotation::cur()));
            let pub_key_hash_rlc = RandomLinearCombination::random_linear_combine_expr(
                pub_key_hash,
                &power_of_randomness,
            );
            table_map.push((selector.clone() * pub_key_hash_rlc, keccak_output_rlc));

            table_map
        });

        // ECDSA config
        let (rns_base, rns_scalar) = GeneralEccChip::<Secp256k1Affine, F>::rns(BIT_LEN_LIMB);
        let main_gate_config = MainGate::<F>::configure(meta);
        let mut overflow_bit_lengths: Vec<usize> = vec![];
        overflow_bit_lengths.extend(rns_base.overflow_lengths());
        overflow_bit_lengths.extend(rns_scalar.overflow_lengths());
        let range_config = RangeChip::<F>::configure(meta, &main_gate_config, overflow_bit_lengths);

        SignVerifyConfig {
            q_enable,
            pub_key_hash,
            address,
            msg_hash_rlc,
            msg_hash_rlc_is_zero,
            msg_hash_rlc_inv,
            range_config,
            main_gate_config,
            pub_key,
            pk_x_limbs,
            pk_y_limbs,
            msg_hash,
            msg_hash_limbs,
            power_of_randomness,
            keccak_table,
        }
    }

    pub fn assign(
        &self,
        config: SignVerifyConfig<F>,
        mut layouter: impl Layouter<F>,
        randomness: F,
        txs: &[TxSignData],
    ) -> Result<(), Error> {
        let mut ecc_chip =
            GeneralEccChip::<Secp256k1Affine, F>::new(config.ecc_chip_config(), BIT_LEN_LIMB);
        let scalar_chip = ecc_chip.scalar_field_chip();

        // Only using 1 signature for now
        let TxSignData {
            signature,
            pub_key,
            msg_hash,
        } = txs[0];
        let (sig_r, sig_s) = signature;
        let pk = pub_key;

        layouter.assign_region(
            || "assign aux values",
            |mut region| {
                let offset = &mut 0;
                let ctx = &mut RegionCtx::new(&mut region, offset);

                ecc_chip.assign_aux_generator(ctx, Some(self.aux_generator))?;
                ecc_chip.assign_aux(ctx, self.window_size, 1)?;
                Ok(())
            },
        )?;

        let ecdsa_chip = EcdsaChip::new(ecc_chip.clone());
        let msg_hash_rlc_is_zero_chip = IsZeroChip::construct(config.msg_hash_rlc_is_zero.clone());

        let mut keccak_auxs = Vec::new();
        layouter.assign_region(
            || "signature verify + ecdsa chip verification witness",
            |mut region| {
                let mut offset = 0;
                let ctx_offset = &mut 0;
                let ctx = &mut RegionCtx::new(&mut region, ctx_offset);

                {
                    let integer_r = ecc_chip.new_unassigned_scalar(Some(sig_r));
                    let integer_s = ecc_chip.new_unassigned_scalar(Some(sig_s));
                    let msg_hash = ecc_chip.new_unassigned_scalar(Some(msg_hash));

                    let r_assigned = scalar_chip.assign_integer(ctx, integer_r)?;
                    let s_assigned = scalar_chip.assign_integer(ctx, integer_s)?;
                    let sig = AssignedEcdsaSig {
                        r: r_assigned,
                        s: s_assigned,
                    };

                    let pk_in_circuit = ecc_chip.assign_point(ctx, Some(pk.into()))?;
                    let pk_assigned = AssignedPublicKey {
                        point: pk_in_circuit,
                    };
                    let msg_hash = scalar_chip.assign_integer(ctx, msg_hash)?;
                    ecdsa_chip.verify(ctx, &sig, &pk_assigned, &msg_hash)?;

                    // Copy constraint between ecdsa verification integers and local columns
                    // copy_integer(&mut region, "sig_r", sig.r, &config.sig_r_limbs, offset)?;
                    // copy_integer(&mut region, "sig_s", sig.s, &config.sig_s_limbs, offset)?;
                    copy_integer(
                        &mut region,
                        "pk_x",
                        pk_assigned.point.get_x(),
                        &config.pk_x_limbs,
                        offset,
                    )?;
                    copy_integer(
                        &mut region,
                        "pk_y",
                        pk_assigned.point.get_y(),
                        &config.pk_y_limbs,
                        offset,
                    )?;
                    copy_integer(
                        &mut region,
                        "msg_hash",
                        msg_hash,
                        &config.msg_hash_limbs,
                        offset,
                    )?;
                }

                config.q_enable.enable(&mut region, offset)?;

                // Assign msg_hash_rlc & msg_hash_rlc_is_zero gadget
                let mut msg_hash_le = [0u8; 32];
                msg_hash
                    .write(&mut Cursor::new(&mut msg_hash_le[..]))
                    .unwrap();
                let msg_hash_rlc = Word::random_linear_combine(msg_hash_le, randomness);
                region.assign_advice(
                    || format!("msg_hash_rlc"),
                    config.msg_hash_rlc,
                    offset,
                    || Ok(msg_hash_rlc),
                )?;
                msg_hash_rlc_is_zero_chip.assign(&mut region, offset, Some(msg_hash_rlc))?;

                // Assign pub_key
                let pk_coord = pk.coordinates().unwrap();
                let mut pk_x_le = [0u8; 32];
                let mut pk_y_le = [0u8; 32];
                pk_coord
                    .x()
                    .write(&mut Cursor::new(&mut pk_x_le[..]))
                    .unwrap();
                pk_coord
                    .y()
                    .write(&mut Cursor::new(&mut pk_y_le[..]))
                    .unwrap();
                for (i, byte) in pk_x_le.iter().enumerate() {
                    // println!("DBG pk x {:02} = {:02x}", i, byte);
                    region.assign_advice(
                        || format!("pub_key x byte {}", i),
                        config.pub_key[i],
                        offset,
                        || Ok(F::from(*byte as u64)),
                    )?;
                }
                for (i, byte) in pk_y_le.iter().enumerate() {
                    // println!("DBG pk y {:02} = {:02x}", i, byte);
                    region.assign_advice(
                        || format!("pub_key y byte {}", i),
                        config.pub_key[32 + i],
                        offset,
                        || Ok(F::from(*byte as u64)),
                    )?;
                }

                let mut pk_x_be = pk_x_le.clone();
                pk_x_be.reverse();
                let mut pk_y_be = pk_y_le.clone();
                pk_y_be.reverse();
                let mut pub_key_bytes_be = [0u8; 64];
                pub_key_bytes_be[..32].copy_from_slice(&pk_x_be);
                pub_key_bytes_be[32..].copy_from_slice(&pk_y_be);
                let mut keccak = Keccak::default();
                keccak.update(&pub_key_bytes_be);
                let pub_key_hash = keccak.digest();

                // Assign pub_key_hash
                for (i, byte) in pub_key_hash.iter().enumerate() {
                    region.assign_advice(
                        || format!("pub_key_hash byte {}", i),
                        config.pub_key_hash[i],
                        offset,
                        || Ok(F::from(*byte as u64)),
                    )?;
                }

                keccak_auxs.push(KeccakAux {
                    input: pub_key_bytes_be,
                    output: pub_key_hash.try_into().unwrap(),
                });

                Ok(())
            },
        )?;

        config.load_keccak(&mut layouter, keccak_auxs, randomness)?;
        config.load_range(&mut layouter)?;

        Ok(())
    }

    /*
    pub fn assign_tx(
        mut layouter: impl Layouter<F>,
        config: Self::Config,
        randomness: F,
        tx: &TxSignData,
    ) -> Result<(), Error> {
        Ok(())
    }
    */

    /*
        pub fn load_tables(config: &SignVerifyConfig<F>, layouter: &mut impl Layouter<F>) {
            use ecdsa::maingate::RangeInstructions;
            const BIT_LEN_LIMB: usize = 68;
            use ecdsa::integer::NUMBER_OF_LOOKUP_LIMBS;

            let bit_len_lookup = BIT_LEN_LIMB / NUMBER_OF_LOOKUP_LIMBS;
            let range_chip = RangeChip::<F>::new(config.range_config.clone(), bit_len_lookup).unwrap();
            range_chip.load_limb_range_table(layouter).unwrap();
            range_chip.load_overflow_range_tables(layouter).unwrap();
       }
    */
}

struct TxSignData {
    signature: (secp256k1::Fq, secp256k1::Fq),
    pub_key: Secp256k1Affine,
    msg_hash: secp256k1::Fq,
}

/*
pub trait SignVerifyInstruction<F: FieldExt> {
    fn check(pub_key_hash: Vec<u8>, address: F, msg_hash_rlc: F, random: F) -> Result<(), Error>;
}
*/

#[cfg(test)]
mod sign_verify_tets {
    use super::*;
    use group::Group;
    use halo2_proofs::dev::MockProver;
    use halo2_proofs::pairing::bn256::Fr;
    use pretty_assertions::assert_eq;
    use rand::RngCore;
    use rand::SeedableRng;
    use rand_xorshift::XorShiftRng;

    #[derive(Clone, Debug)]
    struct TestCircuitSignVerifyConfig<F: FieldExt> {
        sign_verify: SignVerifyConfig<F>,
        /* main_gate_config: MainGateConfig,
         * range_config: RangeConfig,
         * // sig_s_limbs: [Column<Advice>; 4],
         * // sig_r_limbs: [Column<Advice>; 4],
         * pk_x_limbs: [Column<Advice>; 4],
         * pk_y_limbs: [Column<Advice>; 4],
         * msg_hash_limbs: [Column<Advice>; 4], */
    }

    impl<F: FieldExt> TestCircuitSignVerifyConfig<F> {
        pub fn new(meta: &mut ConstraintSystem<F>) -> Self {
            let power_of_randomness = {
                [(); POW_RAND_SIZE].map(|_| meta.instance_column())
                // let columns = [(); POW_RAND_SIZE].map(|_|
                // meta.instance_column());
                // let mut power_of_randomness = None;

                // meta.create_gate("power of randomness", |meta| {
                //     power_of_randomness =
                //         Some(columns.map(|column| meta.query_instance(column,
                // Rotation::cur())));

                //     [0.expr()]
                // });

                // power_of_randomness.unwrap()
            };

            let sign_verify = SignVerifyChip::configure(meta, power_of_randomness);
            TestCircuitSignVerifyConfig { sign_verify }
        }

        // pub fn ecc_chip_config(&self) -> EccConfig {
        //     EccConfig::new(self.range_config.clone(), self.main_gate_config.clone())
        // }

        // pub fn config_range<F: FieldExt>(
        //     &self,
        //     layouter: &mut impl Layouter<F>,
        // ) -> Result<(), Error> {
        //     let bit_len_lookup = BIT_LEN_LIMB / NUMBER_OF_LOOKUP_LIMBS;
        //     let range_chip = RangeChip::<F>::new(self.range_config.clone(),
        // bit_len_lookup);     range_chip.load_limb_range_table(layouter)?;
        //     range_chip.load_overflow_range_tables(layouter)?;

        //     Ok(())
        // }
    }

    #[derive(Default)]
    struct TestCircuitSignVerify<F: FieldExt> {
        sign_verify: SignVerifyChip<F>,
        randomness: F,
        // power_of_randomness: [Expression<F>; POW_RAND_SIZE],
        txs: Vec<TxSignData>,
        /* aux_generator: Secp256k1Affine,
         * window_size: usize,
         * txs: Vec<TxSignData>,
         * _marker: PhantomData<F>, */
    }

    impl<F: FieldExt> Circuit<F> for TestCircuitSignVerify<F> {
        type Config = TestCircuitSignVerifyConfig<F>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            TestCircuitSignVerifyConfig::new(meta)
        }

        fn synthesize(
            &self,
            config: Self::Config,
            layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            self.sign_verify
                .assign(config.sign_verify, layouter, self.randomness, &self.txs)
        }
    }

    const VERIF_HEIGHT: usize = 20_000;

    fn run<F: FieldExt>(txs: Vec<TxSignData>) {
        let k = 20;
        let mut rng = XorShiftRng::seed_from_u64(2);
        let aux_generator =
            <Secp256k1Affine as CurveAffine>::CurveExt::random(&mut rng).to_affine();

        let randomness = F::random(&mut rng);
        let mut power_of_randomness: Vec<Vec<F>> = (1..POW_RAND_SIZE + 1)
            .map(|exp| vec![randomness.pow(&[exp as u64, 0, 0, 0]); txs.len() * VERIF_HEIGHT])
            .collect();
        // SignVerifyChip -> ECDSAChip -> MainGate instance column
        power_of_randomness.push(vec![]);
        // println!("DBG power_of_randomness: {:?}", power_of_randomness);
        let circuit = TestCircuitSignVerify::<F> {
            sign_verify: SignVerifyChip {
                aux_generator,
                window_size: 2,
                _marker: PhantomData,
            },
            randomness,
            txs,
        };

        // let public_inputs = vec![vec![]];
        let prover = match MockProver::run(k, &circuit, power_of_randomness) {
            Ok(prover) => prover,
            Err(e) => panic!("{:#?}", e),
        };
        assert_eq!(prover.verify(), Ok(()));
    }

    // Generate a test key pair
    fn gen_key_pair(rng: impl RngCore) -> (secp256k1::Fq, Secp256k1Affine) {
        // generate a valid signature
        let generator = <Secp256k1Affine as PrimeCurveAffine>::generator();
        let sk = <Secp256k1Affine as CurveAffine>::ScalarExt::random(rng);
        let pk = generator * sk;
        let pk = pk.to_affine();

        (sk, pk)
    }

    // Generate a test message hash
    fn gen_msg_hash(rng: impl RngCore) -> secp256k1::Fq {
        <Secp256k1Affine as CurveAffine>::ScalarExt::random(rng)
    }

    // Returns (r, s)
    fn sign(
        rng: impl RngCore,
        sk: secp256k1::Fq,
        msg_hash: secp256k1::Fq,
    ) -> (secp256k1::Fq, secp256k1::Fq) {
        let randomness = <Secp256k1Affine as CurveAffine>::ScalarExt::random(rng);
        let randomness_inv = randomness.invert().unwrap();
        let generator = <Secp256k1Affine as PrimeCurveAffine>::generator();
        let sig_point = generator * randomness;
        let x = sig_point.to_affine().coordinates().unwrap().x().clone();

        let x_repr = &mut Vec::with_capacity(32);
        x.write(x_repr).unwrap();

        let mut x_bytes = [0u8; 64];
        x_bytes[..32].copy_from_slice(&x_repr[..]);

        let x_bytes_on_n = <Secp256k1Affine as CurveAffine>::ScalarExt::from_bytes_wide(&x_bytes); // get x cordinate (E::Base) on E::Scalar
        let sig_s = randomness_inv * (msg_hash + x_bytes_on_n * sk);
        (x_bytes_on_n, sig_s)
    }

    #[test]
    fn test_sign_verify() {
        let mut rng = XorShiftRng::seed_from_u64(1);
        let (sk, pk) = gen_key_pair(&mut rng);
        let msg_hash = gen_msg_hash(&mut rng);
        let sig = sign(&mut rng, sk, msg_hash);

        let txs = vec![TxSignData {
            signature: sig,
            pub_key: pk,
            msg_hash: msg_hash,
        }];

        // generate a valid signature

        run::<Fr>(txs);
    }
}
