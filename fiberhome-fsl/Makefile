include $(TOPDIR)/rules.mk
include $(INCLUDE_DIR)/kernel.mk

PKG_NAME:=fiberhome-fsl
PKG_RELEASE:=1
PKG_LICENSE:=GPL-2.0-only
PKG_BUILD_DIR:=$(KERNEL_BUILD_DIR)/$(PKG_NAME)

include $(INCLUDE_DIR)/package.mk

define KernelPackage/fiberhome-fsl
  SUBMENU:=Other modules
  TITLE:=FiberHome FSL exclusive SPI transport
  DEPENDS:=@TARGET_airoha_an7581
  KCONFIG:=CONFIG_SPI=y
  FILES:=$(PKG_BUILD_DIR)/fiberhome-fsl.ko
  AUTOLOAD:=$(call AutoProbe,fiberhome-fsl)
endef

define KernelPackage/fiberhome-fsl/description
  Bounded TX/RX SPI messages with exclusive open and device removal handling.
  FSL protocol and optical policy live in openwrt-fttr userspace. Requires an
  explicit fiberhome,an4475-fsl board node.
endef

define Build/Prepare
	$(call Build/Prepare/Default)
	$(CP) ./src/Makefile ./src/fsl.c ./src/fsl-uapi.h $(PKG_BUILD_DIR)/
endef

define Build/Compile
	$(KERNEL_MAKE) M="$(PKG_BUILD_DIR)" modules
endef

$(eval $(call KernelPackage,fiberhome-fsl))
