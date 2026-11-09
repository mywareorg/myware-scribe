//! Microsoft Store purchase client.
//!
//! The paid tier is a durable add-on. Packaged builds ask
//! `Windows.Services.Store` whether it is owned and start the purchase from the
//! paywall; portable builds keep the no-store provider so development and other
//! platforms still run. The gating code only ever sees
//! [`hippo_transcribe_core::license::Provider`].
//!
//! Notes for anyone touching this:
//!
//! * The add-on is found by its Partner Center *product ID* (`pro`), not by the
//!   Store ID, so the ID does not have to be compiled in.
//! * The purchase dialog is owned by the app window: the Desktop Bridge needs
//!   `IInitializeWithWindow`, or the Store reports errors instead of showing UI.
//! * WinRT calls block until they complete, so the price is fetched on a
//!   background thread when the client is created; the paywall only reads the
//!   cached value.

#[cfg(not(windows))]
pub fn provider() -> Option<Box<dyn hippo_transcribe_core::license::Provider>> {
    None
}

#[cfg(not(windows))]
pub fn entitlement_check() -> Option<bool> {
    None
}

#[cfg(windows)]
pub use windows_impl::{entitlement_check, provider};

#[cfg(windows)]
mod windows_impl {
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake, Waker};

    use hippo_transcribe_core::license::{Provider, PurchaseState};
    use windows::core::{Interface, HSTRING};
    use windows::Services::Store::{StoreContext, StoreProduct, StorePurchaseStatus};
    use windows_collections::IIterable;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::IInitializeWithWindow;
    use windows::Win32::System::WinRT::{
        RoInitialize, RO_INIT_MULTITHREADED, RO_INIT_SINGLETHREADED,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    use windows_sys::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE;
    use windows_sys::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;

    /// Product ID entered in Partner Center for the durable Pro add-on.
    const ADD_ON_TOKEN: &str = "pro";
    /// Add-on kind listed from the Store.
    const DURABLE: &str = "Durable";

    /// The Store client, when this is a packaged build with a Store identity.
    pub fn provider() -> Option<Box<dyn Provider>> {
        StoreProvider::new().map(|provider| Box::new(provider) as Box<dyn Provider>)
    }

    /// Background entitlement check. `None` when there is no Store client (a
    /// portable build), `Some(false)` when the Store says the add-on is not
    /// owned.
    pub fn entitlement_check() -> Option<bool> {
        if !packaged() {
            return None;
        }
        find_product()?.IsInUserCollection().ok()
    }

    pub struct StoreProvider {
        /// Formatted price, filled by a background lookup.
        price: Arc<Mutex<Option<String>>>,
    }

    impl StoreProvider {
        fn new() -> Option<Self> {
            if !packaged() {
                return None;
            }
            // The Store client is created on the UI thread; WinRT needs the
            // apartment initialised there. A thread already initialised another
            // way simply keeps its model.
            unsafe {
                let _ = RoInitialize(RO_INIT_SINGLETHREADED);
            }
            // A cheap check that a store context exists at all; the price lookup
            // is what actually reaches the network, and it runs off-thread.
            StoreContext::GetDefault().ok()?;
            let price = Arc::new(Mutex::new(None));
            let slot = Arc::clone(&price);
            std::thread::spawn(move || {
                if let Some(product) = find_product() {
                    if let Ok(value) = product
                        .Price()
                        .and_then(|price| price.FormattedPrice())
                    {
                        if let Ok(mut slot) = slot.lock() {
                            *slot = Some(value.to_string());
                        }
                    }
                }
            });
            Some(Self { price })
        }

        fn owned(&self) -> Option<bool> {
            find_product()?.IsInUserCollection().ok()
        }
    }

    impl Provider for StoreProvider {
        fn name(&self) -> &'static str {
            "microsoft"
        }

        fn price(&self) -> Option<String> {
            self.price.lock().ok().and_then(|price| price.clone())
        }

        fn purchase(&mut self) -> PurchaseState {
            let Some(product) = find_product() else {
                return PurchaseState::Unavailable;
            };
            attach_dialog_owner();
            let result = product
                .RequestPurchaseAsync()
                .ok()
                .and_then(|operation| block_on(async move { operation.await }).ok());
            let Some(result) = result else {
                return PurchaseState::Failed;
            };
            match result.Status() {
                Ok(status)
                    if status == StorePurchaseStatus::Succeeded
                        || status == StorePurchaseStatus::AlreadyPurchased =>
                {
                    // Refresh the cached price: a sale may have ended.
                    self.refresh_price();
                    PurchaseState::Active
                }
                Ok(status) if status == StorePurchaseStatus::NotPurchased => {
                    PurchaseState::Cancelled
                }
                _ => PurchaseState::Failed,
            }
        }

        fn restore(&mut self) -> PurchaseState {
            match self.owned() {
                Some(true) => PurchaseState::Active,
                Some(false) => PurchaseState::Idle,
                None => PurchaseState::Unavailable,
            }
        }

        fn manage_url(&self) -> Option<String> {
            None
        }
    }

    impl StoreProvider {
        fn refresh_price(&self) {
            if let Some(product) = find_product() {
                if let Ok(value) = product.Price().and_then(|price| price.FormattedPrice()) {
                    if let Ok(mut slot) = self.price.lock() {
                        *slot = Some(value.to_string());
                    }
                }
            }
        }
    }

    /// True when the process has a package identity, which is what gives the
    /// Store something to answer about.
    fn packaged() -> bool {
        let mut length = 0u32;
        let status = unsafe { GetCurrentPackageFullName(&mut length, std::ptr::null_mut()) };
        status != APPMODEL_ERROR_NO_PACKAGE
    }

    /// The Pro add-on, found by the product ID it was created with.
    fn find_product() -> Option<StoreProduct> {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let context = StoreContext::GetDefault().ok()?;
        let kinds: IIterable<HSTRING> = IIterable::from(vec![HSTRING::from(DURABLE)]);
        let operation = context.GetAssociatedStoreProductsAsync(&kinds).ok()?;
        let query = block_on(async move { operation.await }).ok()?;
        let products = query.Products().ok()?;
        for pair in &products {
            let product = pair.Value().ok()?;
            let token = product.InAppOfferToken().ok()?;
            if token.to_string() == ADD_ON_TOKEN {
                return Some(product);
            }
        }
        None
    }

    /// Give the Store's dialogs the app window as their owner. Without this a
    /// Desktop Bridge process gets errors instead of a purchase dialog.
    fn attach_dialog_owner() {
        let Ok(context) = StoreContext::GetDefault() else {
            return;
        };
        let Ok(initialize) = context.cast::<IInitializeWithWindow>() else {
            return;
        };
        let window: HWND = unsafe { GetForegroundWindow() };
        if window.is_invalid() {
            return;
        }
        unsafe {
            let _ = initialize.Initialize(window);
        }
    }

    /// Drive a WinRT operation to completion on this thread. The operations used
    /// here are short (a price lookup, an entitlement query) or owned by a modal
    /// Store dialog; the future's completion wakes the parked thread.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct ThreadWaker(std::thread::Thread);
        impl Wake for ThreadWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match std::future::Future::poll(future.as_mut(), &mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::park(),
            }
        }
    }
}
