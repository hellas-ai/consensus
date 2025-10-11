pub trait ConsensusParams {
    const N: usize;
    const F: usize;
    const M_SIZE: usize = 2 * Self::F + 1;
    const L_SIZE: usize = Self::N - Self::F;
}
