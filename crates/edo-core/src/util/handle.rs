#[macro_export]
macro_rules! def_trait {
    ($tdoc: expr => $cdoc: expr => $name: ident : $t: ident {
        $(
            $fn_doc: expr => $fn_name: ident ( $($fn_arg_n: ident : $fn_arg_t: ty),* ) -> $rtype: ty
        );*
        $( : $fn_doc2: expr => $fn_name2: ident ( $($fn_arg_n2: ident : $fn_arg_t2: ty),* ) -> $rtype2: ty );*
    }) => {
        #[doc = $tdoc]
        #[async_trait::async_trait]
        pub trait $t: Send + Sync {
            $(#[doc = $fn_doc]
            async fn $fn_name(&self, $($fn_arg_n : $fn_arg_t),*) -> $rtype;)*
            $(#[doc = $fn_doc2]
            fn $fn_name2(&self, $($fn_arg_n2 : $fn_arg_t2),*) -> $rtype2;)*
        }

        #[derive(Clone)]
        #[doc = $cdoc]
        pub struct $name {
            inner: std::sync::Arc<dyn $t>,
        }

        unsafe impl Send for $name {}
        unsafe impl Sync for $name {}

        impl $name {
            pub fn from_impl(inner: impl $t + 'static) -> Self {
                Self {
                    inner: std::sync::Arc::new(inner),
                }
            }

            $(
                #[doc = $fn_doc]
                pub async fn $fn_name(&self, $($fn_arg_n : $fn_arg_t),*) -> $rtype {
                    self.inner.$fn_name($($fn_arg_n),*).await
                }
            )*

            $(
                #[doc = $fn_doc2]
                pub fn $fn_name2(&self, $($fn_arg_n2 : $fn_arg_t2),*) -> $rtype2 {
                    self.inner.$fn_name2($($fn_arg_n2),*)
                }
            )*
        }
    };
}
