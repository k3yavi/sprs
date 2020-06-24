//! Implementation of the paper
//! Bank and Douglas, 2001, Sparse Matrix Multiplication Package (SMPP)

use indexing::SpIndex;
use num_traits::Num;
use rayon::prelude::*;
use sparse::prelude::*;
use sparse::CompressedStorage::CSR;

use std::cell::RefCell;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SymbMethod {
    LinkedList,
    BoolVecAndSort,
}

thread_local!(static SYMBOLIC_METHOD: RefCell<SymbMethod> =
    RefCell::new(SymbMethod::BoolVecAndSort)
);

/// Set the method used to compute the symbolic part of sparse matrix
/// product, for the current thread. This will affect all subsequent calls
/// to the `*` operator between sparse matrices for the current thread until
/// it is changed again.
pub fn set_thread_symbolic_method(method: SymbMethod) {
    SYMBOLIC_METHOD.with(|m| {
        *m.borrow_mut() = method;
    });
}

/// Get the current method for computing the symbolic part of sparse matrix
/// product.
pub fn thread_symbolic_method() -> SymbMethod {
    SYMBOLIC_METHOD.with(|m| *m.borrow())
}

/// Control the strategy used to parallelize the matrix product workload.
///
/// The `Automatic` strategy will try to pick a good number of threads based
/// on the number of physical cores and an estimation of the nnz of the product
/// matrix.
///
/// The `Fixed` strategy leaves the control to the user. It is a programming
/// error to request 0 threads.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ThreadingStrategy {
    Automatic,
    Fixed(usize),
}

thread_local!(static THREADING_STRAT: RefCell<ThreadingStrategy> =
    RefCell::new(ThreadingStrategy::Automatic)
);

/// Set the threading strategy for matrix products in this thread.
///
/// # Panics
///
/// If a number of 0 threads is requested.
pub fn set_thread_threading_strategy(strategy: ThreadingStrategy) {
    if let ThreadingStrategy::Fixed(nb_threads) = strategy {
        assert!(nb_threads > 0);
    }
    THREADING_STRAT.with(|s| {
        *s.borrow_mut() = strategy;
    });
}

pub fn thread_threading_strategy() -> ThreadingStrategy {
    THREADING_STRAT.with(|s| *s.borrow())
}

/// Compute the symbolic structure of the matrix product C = A * B, with
/// A, B and C stored in the CSR matrix format.
///
/// This algorithm has a complexity of O(n * k * log(k)), where k is the
/// average number of nonzeros in the rows of the result.
///
/// # Panics
///
/// `index.len()` should be equal to the maximum dimension among the input
/// matrices.
///
/// The matrices should be in proper CSR structure, and their dimensions
/// should be compatible. Failures to do so may result in out of bounds errors
/// (though some cases might go unnoticed).
///
/// # Minimizing allocations
///
/// This function will reserve
/// `a_indptr.last().unwrap() + b_indptr.last.unwrap()` in `c_indices`.
/// Therefore, to prevent this function from allocating, it is required
/// to have reserved at least this amount of memory.
pub fn symbolic<Iptr: SpIndex, I: SpIndex>(
    a_indptr: &[Iptr],
    a_indices: &[I],
    b_cols: usize,
    b_indptr: &[Iptr],
    b_indices: &[I],
    c_indptr: &mut [Iptr],
    // TODO look for litterature on the nnz of C to be able to have a slice here
    c_indices: &mut Vec<I>,
    index: &mut [usize],
) {
    assert!(a_indptr.len() == c_indptr.len());
    let a_rows = a_indptr.len() - 1;
    let b_rows = b_indptr.len() - 1;
    let a_nnz = a_indptr[a_rows].index();
    let b_nnz = b_indptr[b_rows].index();
    c_indices.clear();
    c_indices.reserve_exact(a_nnz + b_nnz);

    // `index` is used as a set to remember which columns of a row of C are
    // nonzero. At any point in the algorithm, if `index[col] == sentinel0`,
    // then we know there is no nonzero value in the column. As the algorithm
    // progresses, we discover nonzero elements. When a nonzero at `col` is
    // discovered, we store in `index[col]` the column of the preceding
    // nonzero (storing `sentinel1` for the first nonzero). Therefore,
    // when we want to collect nonzeros and clear the set, we can simply
    // follow the trail of column indices, putting back `sentinel0` along
    // the way. This way, collecting the nonzero indices for a column
    // has a complexity O(col_nnz).
    let ind_len = a_rows.max(b_rows.max(b_cols));
    let sentinel0 = usize::max_value();
    let sentinel1 = usize::max_value() - 1;
    assert!(index.len() == ind_len);
    assert!(ind_len < sentinel1);
    for elt in index.iter_mut() {
        *elt = sentinel0;
    }

    c_indptr[0] = Iptr::from_usize(0);
    for a_row in 0..a_rows {
        let mut istart = sentinel1;
        let mut length = 0;
        let mut max_bcol = 0;
        let mut min_bcol = index.len();

        let a_start = a_indptr[a_row].index();
        let a_stop = a_indptr[a_row + 1].index();
        for &a_col in &a_indices[a_start..a_stop] {
            let b_row = a_col.index();
            let b_start = b_indptr[b_row].index();
            let b_stop = b_indptr[b_row + 1].index();
            for b_col in &b_indices[b_start..b_stop] {
                let b_col = b_col.index();
                if index[b_col] == sentinel0 {
                    index[b_col] = istart;
                    istart = b_col;
                    length += 1;
                    max_bcol = max_bcol.max(b_col);
                    min_bcol = min_bcol.min(b_col);
                }
            }
        }
        c_indptr[a_row + 1] = c_indptr[a_row] + Iptr::from_usize(length);
        // We dynamically change our strategy to recover the nonzero indices
        // sorted. We try and determine if it's cheaper to scan
        // `index[min_col..=max_col]` or to use the linked list embedded in
        // the array and sort the indices afterwards. In essence, we're
        // choosing between `max_col - min_col` comparisons vs
        // `length * log(length)` comparisons.
        // TODO: the linear scan could be implemented faster using SIMD
        if max_bcol > min_bcol && max_bcol - min_bcol < length * log2(length) {
            for b_col in min_bcol..=max_bcol {
                if index[b_col] != sentinel0 {
                    c_indices.push(I::from_usize(b_col));
                    index[b_col] = sentinel0;
                }
            }
            debug_assert_eq!(c_indices.len(), c_indptr[a_row + 1].index());
        } else {
            for _ in 0..length {
                debug_assert!(istart < sentinel1);
                c_indices.push(I::from_usize(istart));
                let new_start = index[istart];
                index[istart] = sentinel0;
                istart = new_start;
            }
            let c_start = c_indptr[a_row].index();
            let c_end = c_indptr[a_row + 1].index();
            c_indices[c_start..c_end].sort_unstable();
        }
        index[a_row] = sentinel0;
    }
}

pub fn symbolic_boolvec<Iptr: SpIndex, I: SpIndex>(
    a_indptr: &[Iptr],
    a_indices: &[I],
    b_cols: usize,
    b_indptr: &[Iptr],
    b_indices: &[I],
    c_indptr: &mut [Iptr],
    // TODO look for litterature on the nnz of C to be able to have a slice here
    c_indices: &mut Vec<I>,
    seen: &mut [bool],
) {
    assert!(a_indptr.len() == c_indptr.len());
    let a_rows = a_indptr.len() - 1;
    let b_rows = b_indptr.len() - 1;
    let a_nnz = a_indptr[a_rows].index();
    let b_nnz = b_indptr[b_rows].index();
    c_indices.clear();
    c_indices.reserve_exact(a_nnz + b_nnz);

    let ind_len = a_rows.max(b_rows.max(b_cols));
    assert!(seen.len() == ind_len);
    for elt in seen.iter_mut() {
        *elt = false;
    }

    c_indptr[0] = Iptr::from_usize(0);
    for a_row in 0..a_rows {
        let mut length = 0;

        let a_start = a_indptr[a_row].index();
        let a_stop = a_indptr[a_row + 1].index();
        for &a_col in &a_indices[a_start..a_stop] {
            let b_row = a_col.index();
            let b_start = b_indptr[b_row].index();
            let b_stop = b_indptr[b_row + 1].index();
            for b_col in &b_indices[b_start..b_stop] {
                let b_col = b_col.index();
                if !seen[b_col] {
                    seen[b_col] = true;
                    c_indices.push(I::from_usize(b_col));
                    length += 1;
                }
            }
        }
        c_indptr[a_row + 1] = c_indptr[a_row] + Iptr::from_usize(length);
        let c_start = c_indptr[a_row].index();
        let c_end = c_start + length;
        c_indices[c_start..c_end].sort_unstable();
        for c_col in &c_indices[c_start..c_end] {
            seen[c_col.index()] = false;
        }
    }
}

/// Compute the approximate base 2 logarithm of an integer, using its
/// number of "used" bits.
fn log2(num: usize) -> usize {
    let num_bits = std::mem::size_of::<usize>() * 8;
    assert!(num > 0);
    num_bits - (num.leading_zeros() as usize) - 1
}

/// Numeric part of the matrix product C = A * B with A, B and C stored in the
/// CSR matrix format.
///
/// This function is low-level, and supports execution on chunks of the
/// rows of C and A. To use the chunks, split the indptrs of A and C and split
/// `c_indices` and `c_data` to only contain the elements referenced in
/// `c_indptr`. This function will take care of using the correct offset
/// inside the sliced indices and data.
///
/// # Panics
///
/// `tmp.len()` should be equal to the maximum dimension of the inputs.
///
/// The matrices should be in proper CSR structure, and their dimensions
/// should be compatible. Failures to do so may result in out of bounds errors
/// (though some cases might go unnoticed).
///
/// The parts for the C matrix should come from the `symbolic` function.
pub fn numeric<
    Iptr: SpIndex,
    I: SpIndex,
    N: Num + Copy + std::ops::AddAssign,
>(
    a_indptr: &[Iptr],
    a_indices: &[I],
    a_data: &[N],
    b_indptr: &[Iptr],
    b_indices: &[I],
    b_data: &[N],
    c_indptr: &[Iptr],
    c_indices: &[I],
    c_data: &mut [N],
    tmp: &mut [N],
) {
    assert!(a_indptr.len() == c_indptr.len());
    let a_rows = a_indptr.len() - 1;
    let b_rows = b_indptr.len() - 1;
    assert!(b_indices.len() == b_indptr[b_rows].index());
    assert!(b_data.len() == b_indptr[b_rows].index());

    for elt in tmp.iter_mut() {
        *elt = N::zero();
    }
    for a_row in 0..a_rows {
        let a_start = a_indptr[a_row].index();
        let a_stop = a_indptr[a_row + 1].index();
        for a_cur in a_start..a_stop {
            let a_col = a_indices[a_cur].index();
            let a_val = a_data[a_cur];
            let b_row = a_col;
            let b_start = b_indptr[b_row].index();
            let b_stop = b_indptr[b_row + 1].index();
            for b_cur in b_start..b_stop {
                let b_col = b_indices[b_cur].index();
                let b_val = b_data[b_cur];
                tmp[b_col] += a_val * b_val;
            }
        }
        let c_row = a_row;
        let c_start = c_indptr[c_row].index();
        let c_stop = c_indptr[c_row + 1].index();
        for c_cur in c_start..c_stop {
            // Handle split data. On non split data this is a no-op, but on
            // split data this will compute the correct offset.
            let c_cur = c_cur - c_indptr[0].index();
            let c_col = c_indices[c_cur].index();
            c_data[c_cur] = tmp[c_col];
            tmp[c_col] = N::zero();
        }
    }
}

/// Compute a sparse matrix product using the SMMP routines
///
/// # Panics
///
/// - if `lhs.cols() != rhs.rows()`.
pub fn mul_csr_csr<N, I, Iptr>(
    lhs: CsMatViewI<N, I, Iptr>,
    rhs: CsMatViewI<N, I, Iptr>,
) -> CsMatI<N, I, Iptr>
where
    N: Num + Copy + std::ops::AddAssign + Send + Sync,
    I: SpIndex,
    Iptr: SpIndex,
{
    let l_rows = lhs.rows();
    let l_cols = lhs.cols();
    let r_rows = rhs.rows();
    let r_cols = rhs.cols();
    assert_eq!(l_cols, r_rows);
    let method = thread_symbolic_method();
    let workspace_len = l_rows.max(l_cols).max(r_cols);
    let nb_threads = match thread_threading_strategy() {
        ThreadingStrategy::Fixed(nb_threads) => nb_threads,
        ThreadingStrategy::Automatic => {
            let nb_cpus = num_cpus::get_physical();
            let ideal_chunk_size = 8128;
            let wanted_threads = (lhs.nnz() + rhs.nnz()) / ideal_chunk_size;
            1.max(wanted_threads).min(nb_cpus)
        }
    };
    let mut tmps = Vec::with_capacity(nb_threads);
    for _ in 0..nb_threads {
        tmps.push(vec![N::zero(); workspace_len].into_boxed_slice())
    }
    match method {
        SymbMethod::LinkedList => {
            let mut index = vec![0; workspace_len];
            mul_csr_csr_with_workspace(lhs, rhs, &mut index, &mut tmps[0])
        }
        SymbMethod::BoolVecAndSort => {
            let mut seen = vec![false; workspace_len];
            mul_csr_csr_with_workspace_boolvec(lhs, rhs, &mut seen, &mut tmps)
        }
    }
}

/// Compute a sparse matrix product using the SMMP routines, using temporary
/// storage that was already allocated
///
/// `index` and `tmp` are temporary storage vectors used to accumulate non
/// zero locations and values. Their values need not be specified on input.
/// They will be zero on output.
///
/// # Panics
///
/// - if `lhs.cols() != rhs.rows()`.
/// - if `index.len() != lhs.cols().max(lhs.rows()).max(rhs.cols())`
/// - if `tmp.len() != lhs.cols().max(lhs.rows()).max(rhs.cols())`
pub fn mul_csr_csr_with_workspace<N, I, Iptr>(
    lhs: CsMatViewI<N, I, Iptr>,
    rhs: CsMatViewI<N, I, Iptr>,
    index: &mut [usize],
    tmp: &mut [N],
) -> CsMatI<N, I, Iptr>
where
    N: Num + Copy + std::ops::AddAssign,
    I: SpIndex,
    Iptr: SpIndex,
{
    let l_rows = lhs.rows();
    let l_cols = lhs.cols();
    let r_rows = rhs.rows();
    let r_cols = rhs.cols();
    assert_eq!(l_cols, r_rows);
    assert_eq!(index.len(), l_rows.max(l_cols).max(r_cols));
    assert_eq!(tmp.len(), l_rows.max(l_cols).max(r_cols));
    let mut res_indptr = vec![Iptr::zero(); l_rows + 1];
    let mut res_indices = Vec::new();
    symbolic(
        lhs.indptr(),
        lhs.indices(),
        r_cols,
        rhs.indptr(),
        rhs.indices(),
        &mut res_indptr,
        &mut res_indices,
        index,
    );
    let mut res_data = vec![N::zero(); res_indices.len()];
    numeric(
        lhs.indptr(),
        lhs.indices(),
        lhs.data(),
        rhs.indptr(),
        rhs.indices(),
        rhs.data(),
        &res_indptr,
        &res_indices,
        &mut res_data,
        tmp,
    );
    // Correctness: The invariants of the output come from the invariants of
    // the inputs when in-bounds indices are concerned, and we are sorting
    // indices.
    CsMatI::new_trusted(
        CSR,
        (l_rows, r_cols),
        res_indptr,
        res_indices,
        res_data,
    )
}

pub fn mul_csr_csr_with_workspace_boolvec<N, I, Iptr>(
    lhs: CsMatViewI<N, I, Iptr>,
    rhs: CsMatViewI<N, I, Iptr>,
    seen: &mut [bool],
    tmps: &mut [Box<[N]>],
) -> CsMatI<N, I, Iptr>
where
    N: Num + Copy + std::ops::AddAssign + Send + Sync,
    I: SpIndex,
    Iptr: SpIndex,
{
    let l_rows = lhs.rows();
    let l_cols = lhs.cols();
    let r_rows = rhs.rows();
    let r_cols = rhs.cols();
    let workspace_len = l_rows.max(l_cols).max(r_cols);
    assert_eq!(l_cols, r_rows);
    assert_eq!(seen.len(), workspace_len);
    assert!(tmps.iter().all(|x| x.len() == workspace_len));
    let mut res_indptr = vec![Iptr::zero(); l_rows + 1];
    let mut res_indices = Vec::new();
    symbolic_boolvec(
        lhs.indptr(),
        lhs.indices(),
        r_cols,
        rhs.indptr(),
        rhs.indices(),
        &mut res_indptr,
        &mut res_indices,
        seen,
    );
    let mut res_data = vec![N::zero(); res_indices.len()];
    let nb_threads = tmps.len();
    let chunk_size = res_indices.len() / nb_threads;
    let mut res_indices_rem = &res_indices[..];
    let mut res_data_rem = &mut res_data[..];
    let mut prev_nnz = 0;
    let mut split_nnz = 0;
    let mut split_row = 0;
    let mut lhs_indptr_chunks = Vec::with_capacity(nb_threads);
    let mut res_indptr_chunks = Vec::with_capacity(nb_threads);
    let mut res_indices_chunks = Vec::with_capacity(nb_threads);
    let mut res_data_chunks = Vec::with_capacity(nb_threads);
    for (row, nnz) in res_indptr.iter().enumerate() {
        let nnz = nnz.index();
        if nnz - split_nnz > chunk_size && row > 0 {
            lhs_indptr_chunks.push(&lhs.indptr()[split_row..row]);

            res_indptr_chunks.push(&res_indptr[split_row..row]);

            let (left, right) = res_indices_rem
                .split_at(prev_nnz - res_indptr[split_row].index());
            res_indices_chunks.push(left);
            res_indices_rem = right;

            let (left, right) = res_data_rem
                .split_at_mut(prev_nnz - res_indptr[split_row].index());
            res_data_chunks.push(left);
            res_data_rem = right;

            split_nnz = nnz;
            split_row = row - 1;
        }
        prev_nnz = nnz;
    }
    lhs_indptr_chunks.push(&lhs.indptr()[split_row..]);
    res_indptr_chunks.push(&res_indptr[split_row..]);
    res_indices_chunks.push(res_indices_rem);
    res_data_chunks.push(res_data_rem);
    lhs_indptr_chunks
        .par_iter()
        .zip(res_indptr_chunks.par_iter())
        .zip(res_indices_chunks.par_iter())
        .zip(res_data_chunks.par_iter_mut())
        .zip(tmps.par_iter_mut())
        .for_each(
            |(
                (
                    ((lhs_indptr_chunk, res_indptr_chunk), res_indices_chunk),
                    res_data_chunk,
                ),
                tmp,
            )| {
                numeric(
                    lhs_indptr_chunk,
                    lhs.indices(),
                    lhs.data(),
                    rhs.indptr(),
                    rhs.indices(),
                    rhs.data(),
                    res_indptr_chunk,
                    res_indices_chunk,
                    res_data_chunk,
                    tmp,
                );
            },
        );

    // Correctness: The invariants of the output come from the invariants of
    // the inputs when in-bounds indices are concerned, and we are sorting
    // indices.
    CsMatI::new_trusted(
        CSR,
        (l_rows, r_cols),
        res_indptr,
        res_indices,
        res_data,
    )
}

#[cfg(test)]
mod test {
    use test_data;

    #[test]
    fn symbolic_and_numeric() {
        let a = test_data::mat1();
        let b = test_data::mat2();
        // a * b 's structure:
        //                | x x x   x |
        //                | x     x   |
        //                |           |
        //                |     x x   |
        //                |   x x     |
        //
        // |     x x   |  |     x x   |
        // |       x x |  |   x x x   |
        // |     x     |  |           |
        // |   x       |  | x     x   |
        // |       x   |  |     x x   |
        let exp = test_data::mat1_matprod_mat2();

        let mut c_indptr = [0; 6];
        let mut c_indices = Vec::new();
        let mut index = [0; 5];

        super::symbolic(
            a.indptr(),
            a.indices(),
            b.cols(),
            b.indptr(),
            b.indices(),
            &mut c_indptr,
            &mut c_indices,
            &mut index,
        );

        let mut c_data = vec![0.; c_indices.len()];
        let mut tmp = [0.; 5];
        super::numeric(
            a.indptr(),
            a.indices(),
            a.data(),
            b.indptr(),
            b.indices(),
            b.data(),
            &c_indptr,
            &c_indices,
            &mut c_data,
            &mut tmp,
        );
        assert_eq!(exp.indptr(), c_indptr);
        assert_eq!(exp.indices(), &c_indices[..]);
        assert_eq!(exp.data(), &c_data[..]);
    }

    #[test]
    fn mul_csr_csr() {
        let a = test_data::mat1();
        let exp = test_data::mat1_self_matprod();
        let res = super::mul_csr_csr(a.view(), a.view());
        assert_eq!(exp, res);
    }

    #[test]
    fn mul_csr_csr_multithreaded() {
        let a = test_data::mat1();
        let exp = test_data::mat1_self_matprod();
        super::set_thread_threading_strategy(super::ThreadingStrategy::Fixed(
            4,
        ));
        let res = super::mul_csr_csr(a.view(), a.view());
        assert_eq!(exp, res);
    }
}
