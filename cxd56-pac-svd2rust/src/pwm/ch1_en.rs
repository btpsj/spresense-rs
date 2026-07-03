///Register `CH1_EN` reader
pub type R = crate::R<Ch1EnSpec>;
///Register `CH1_EN` writer
pub type W = crate::W<Ch1EnSpec>;
///Field `ENABLE` reader - PWM output enable: 0=disabled (LOW), 1=enabled
pub type EnableR = crate::BitReader;
///Field `ENABLE` writer - PWM output enable: 0=disabled (LOW), 1=enabled
pub type EnableW<'a, REG> = crate::BitWriter<'a, REG>;
impl R {
    ///Bit 0 - PWM output enable: 0=disabled (LOW), 1=enabled
    #[inline(always)]
    pub fn enable(&self) -> EnableR {
        EnableR::new((self.bits & 1) != 0)
    }
}
impl W {
    ///Bit 0 - PWM output enable: 0=disabled (LOW), 1=enabled
    #[inline(always)]
    pub fn enable(&mut self) -> EnableW<'_, Ch1EnSpec> {
        EnableW::new(self, 0)
    }
}
/**CH1 output enable

You can [`read`](crate::Reg::read) this register and get [`ch1_en::R`](R). You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_en::W`](W). You can also [`modify`](crate::Reg::modify) this register. See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch1EnSpec;
impl crate::RegisterSpec for Ch1EnSpec {
    type Ux = u32;
}
///`read()` method returns [`ch1_en::R`](R) reader structure
impl crate::Readable for Ch1EnSpec {}
///`write(|w| ..)` method takes [`ch1_en::W`](W) writer structure
impl crate::Writable for Ch1EnSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH1_EN to value 0
impl crate::Resettable for Ch1EnSpec {}
