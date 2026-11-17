//! 分页参数校验：双轨页码上界的唯一入口。
//!
//! C 端 [`crate::limits::USER_PAGE_MAX`] 与后台 [`crate::limits::ADMIN_PAGE_MAX`]
//! 由调用方按端点选轨；字段级错误供调用方映射各自的错误契约。

/// 校验通过的分页参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    pub page: u32,
    pub page_size: u32,
}

/// 分页越界字段（调用方据此返回各自的既有错误文案）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageBoundsField {
    Page,
    PageSize,
}

impl PageRequest {
    /// C 端轨：页码 1..=10 万。
    pub fn user(page: i64, page_size: i64) -> Result<Self, PageBoundsField> {
        Self::validate(page, page_size, crate::limits::USER_PAGE_MAX)
    }

    /// 后台轨：页码 1..=100 万。
    pub fn admin(page: i64, page_size: i64) -> Result<Self, PageBoundsField> {
        Self::validate(page, page_size, crate::limits::ADMIN_PAGE_MAX)
    }

    fn validate(page: i64, page_size: i64, page_max: i64) -> Result<Self, PageBoundsField> {
        if !(1..=page_max).contains(&page) {
            return Err(PageBoundsField::Page);
        }
        if !(1..=crate::limits::PAGE_SIZE_MAX).contains(&page_size) {
            return Err(PageBoundsField::PageSize);
        }
        Ok(Self {
            page: u32::try_from(page).map_err(|_| PageBoundsField::Page)?,
            page_size: u32::try_from(page_size).map_err(|_| PageBoundsField::PageSize)?,
        })
    }
}