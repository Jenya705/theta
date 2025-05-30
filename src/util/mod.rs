pub mod channel;
pub mod slot_map;

#[macro_export]
macro_rules! ret_type {
    ($expr:expr,$ty:ty) => {{
        let v: $ty = $expr;
        v
    }};
}

#[macro_export]
macro_rules! clone_all {
    ($($val: expr),*$(,)?)=>{
        (
            $(<_ as Clone>::clone($val),)*
        )
    }
}