///Register `CH1_UPDATE` writer
pub type W = crate::W<Ch1UpdateSpec>;
impl core::fmt::Debug for crate::generic::Reg<Ch1UpdateSpec> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "(not readable)")
    }
}
impl W {}
/**CH1 update trigger

You can [`reset`](crate::Reg::reset), [`write`](crate::Reg::write), [`write_with_zero`](crate::Reg::write_with_zero) this register using [`ch1_update::W`](W). See [API](https://docs.rs/svd2rust/#read--modify--write-api).*/
pub struct Ch1UpdateSpec;
impl crate::RegisterSpec for Ch1UpdateSpec {
    type Ux = u32;
}
///`write(|w| ..)` method takes [`ch1_update::W`](W) writer structure
impl crate::Writable for Ch1UpdateSpec {
    type Safety = crate::Unsafe;
}
///`reset()` method sets CH1_UPDATE to value 0
impl crate::Resettable for Ch1UpdateSpec {}
